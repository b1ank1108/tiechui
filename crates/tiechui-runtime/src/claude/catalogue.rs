//! Claude's models and its `/` menu, from `initialize`, `system/commands_changed` and the skills
//! `system/init` names.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use tiechui_protocol::{CommandInfo, CommandKind, InputContent, ModelInfo, ThinkingLevel};

use super::items;
use super::wire;

/// Kept across the processes of one session: each answers `initialize` with it again.
#[derive(Clone, Default)]
pub struct SharedCatalogue(Arc<Mutex<Catalogue>>);

impl SharedCatalogue {
    pub fn lock(&self) -> MutexGuard<'_, Catalogue> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[derive(Default)]
pub struct Catalogue {
    models: Vec<wire::Model>,
    commands: Vec<wire::Command>,
    loaded_skills: BTreeSet<String>,
    skill_roots: Vec<PathBuf>,
    menu: Vec<CommandInfo>,
}

impl Catalogue {
    pub fn set_skill_roots(&mut self, roots: Vec<PathBuf>) {
        self.skill_roots = roots;
    }

    pub fn initialized(&mut self, listed: wire::Initialized) -> (Vec<ModelInfo>, Vec<CommandInfo>) {
        self.models = listed.models;
        let models = self.models.iter().map(model_info).collect();
        (models, self.set_commands(listed.commands))
    }

    pub fn set_commands(&mut self, commands: Vec<wire::Command>) -> Vec<CommandInfo> {
        self.commands = commands;
        self.menu = self.classify();
        self.menu.clone()
    }

    /// The new menu when the skills Claude loaded change what an entry is.
    pub fn loaded(&mut self, skills: Vec<String>) -> Option<Vec<CommandInfo>> {
        let skills: BTreeSet<String> = skills.into_iter().collect();
        if skills == self.loaded_skills {
            return None;
        }
        self.loaded_skills = skills;
        let menu = self.classify();
        if menu == self.menu {
            return None;
        }
        self.menu = menu;
        Some(self.menu.clone())
    }

    /// A built-in is no menu entry; `user-invocable: false` skills Claude leaves out itself.
    fn classify(&self) -> Vec<CommandInfo> {
        self.commands
            .iter()
            .filter(|command| !command.builtin)
            .map(|command| CommandInfo {
                name: command.name.clone(),
                description: command.description.clone(),
                argument_hint: Some(command.argument_hint.clone()).filter(|hint| !hint.is_empty()),
                kind: if self.has_skill(&command.name) {
                    CommandKind::Skill
                } else {
                    CommandKind::Command
                },
            })
            .collect()
    }

    /// A namespaced name (`plugin:skill`, `dir:command`) has no directory of its own.
    fn has_skill(&self, name: &str) -> bool {
        self.loaded_skills.contains(name)
            || !name.contains(['/', '\\', ':'])
                && self
                    .skill_roots
                    .iter()
                    .any(|root| root.join(name).join("SKILL.md").is_file())
    }

    /// Claude accepts a model by its alias or by the model the alias resolves to.
    pub fn model(&self, name: &str) -> Option<ModelInfo> {
        self.models
            .iter()
            .find(|model| model.value == name)
            .or_else(|| {
                self.models
                    .iter()
                    .find(|model| model.resolved_model == name)
            })
            .map(model_info)
    }

    pub fn offers(&self, name: &str) -> bool {
        self.menu.iter().any(|entry| entry.name == name)
    }

    /// The content as Claude should get it. Claude expands a skill or command only when its
    /// `/<name>` opens the last content block, and takes all text after the name as arguments:
    /// a text that already opens with a name Claude lists stays as typed, otherwise the last
    /// skill named as `/<name>` or `$<name>` moves to its front. The text goes after any image.
    pub fn dispatch(&self, content: &[InputContent]) -> Vec<InputContent> {
        let Some((index, text)) =
            content
                .iter()
                .enumerate()
                .rev()
                .find_map(|(index, part)| match part {
                    InputContent::Text { text } => Some((index, text)),
                    InputContent::Image { .. } => None,
                })
        else {
            return content.to_vec();
        };
        let Some(command) = self.command_text(text) else {
            return content.to_vec();
        };
        let mut parts: Vec<InputContent> = content
            .iter()
            .enumerate()
            .filter(|(at, _)| *at != index)
            .map(|(_, part)| part.clone())
            .collect();
        parts.push(InputContent::Text { text: command });
        parts
    }

    fn command_text(&self, text: &str) -> Option<String> {
        if let Some(after_slash) = text.trim_start().strip_prefix('/')
            && mentioned(after_slash, self.listed_names()).is_some()
        {
            return Some(text.to_owned());
        }
        let (start, name) = self.last_skill_mention(text)?;
        let (before, after) = (&text[..start], &text[start + 1 + name.len()..]);
        let rest = if after.is_empty() || after.starts_with(char::is_whitespace) {
            format!("{}{after}", before.trim_end())
        } else {
            format!("{before}{after}")
        };
        let rest = rest.trim();
        Some(if rest.is_empty() {
            format!("/{name}")
        } else {
            format!("/{name} {rest}")
        })
    }

    fn listed_names(&self) -> impl Iterator<Item = &str> {
        self.commands.iter().flat_map(|command| {
            std::iter::once(command.name.as_str()).chain(command.aliases.iter().map(String::as_str))
        })
    }

    fn last_skill_mention<'a>(&'a self, text: &str) -> Option<(usize, &'a str)> {
        let skills = || {
            self.menu
                .iter()
                .filter(|entry| entry.kind == CommandKind::Skill)
                .map(|entry| entry.name.as_str())
        };
        let mut last = None;
        let mut word_start = true;
        let mut at = 0;
        while let Some(c) = text[at..].chars().next() {
            if word_start
                && matches!(c, '/' | '$')
                && let Some(name) = mentioned(&text[at + 1..], skills())
            {
                last = Some((at, name));
                at += 1 + name.len();
                word_start = false;
                continue;
            }
            word_start = c.is_whitespace();
            at += c.len_utf8();
        }
        last
    }
}

/// The longest name `text` starts with as a whole word: not followed by a character that would
/// continue a name or a path.
fn mentioned<'a>(text: &str, names: impl Iterator<Item = &'a str>) -> Option<&'a str> {
    names
        .filter(|name| {
            !name.is_empty()
                && text.strip_prefix(name).is_some_and(|after| {
                    !after
                        .starts_with(|c: char| c.is_alphanumeric() || matches!(c, '-' | '_' | '/'))
                })
        })
        .max_by_key(|name| name.len())
}

/// `off` comes first although Claude reports `supportsThinkingOff: false` for every model: a zero
/// thinking-token cap turns thinking off for any of them. Effort levels tiechui has no name for
/// are left out.
fn model_info(model: &wire::Model) -> ModelInfo {
    let mut levels = vec![ThinkingLevel::Off];
    levels.extend(
        model
            .supported_effort_levels
            .iter()
            .filter_map(|effort| items::effort_level(effort)),
    );
    ModelInfo {
        provider: "claude".into(),
        id: model.value.clone(),
        context_window: 0,
        reasoning: model.supports_effort,
        thinking_levels: levels,
    }
}

/// Where Claude finds skills, as `<root>/<name>/SKILL.md`: the managed directory, the user's
/// config directory (`$CLAUDE_CONFIG_DIR`, else `~/.claude`) and `.claude/skills` in the working
/// directory and each parent up to the repository root. Plugin skills are known by name once
/// `system/init` lists them.
pub fn skill_roots(cwd: &Path) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = [managed_dir(), config_dir(cwd)]
        .into_iter()
        .flatten()
        .map(|dir| dir.join("skills"))
        .collect();
    let repository = cwd.ancestors().position(|dir| dir.join(".git").exists());
    roots.extend(
        cwd.ancestors()
            .take(repository.map_or(1, |index| index + 1))
            .map(|dir| dir.join(".claude").join("skills")),
    );
    roots
}

fn config_dir(cwd: &Path) -> Option<PathBuf> {
    let configured = std::env::var_os("CLAUDE_CONFIG_DIR").filter(|dir| !dir.is_empty());
    if let Some(dir) = configured {
        return Some(cwd.join(dir));
    }
    let home = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(home)
        .filter(|dir| !dir.is_empty())
        .map(|dir| PathBuf::from(dir).join(".claude"))
}

fn managed_dir() -> Option<PathBuf> {
    let base = if cfg!(target_os = "macos") {
        PathBuf::from("/Library/Application Support/ClaudeCode")
    } else if cfg!(windows) {
        PathBuf::from(std::env::var_os("ProgramFiles")?).join("ClaudeCode")
    } else {
        PathBuf::from("/etc/claude-code")
    };
    Some(base.join(".claude"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(name: &str, builtin: bool) -> wire::Command {
        wire::Command {
            name: name.into(),
            builtin,
            ..wire::Command::default()
        }
    }

    fn catalogue() -> Catalogue {
        let mut catalogue = Catalogue::default();
        catalogue.set_commands(
            ["fix", "fix-all", "ns:tool", "deploy"]
                .into_iter()
                .map(|name| command(name, false))
                .chain([command("compact", true)])
                .collect(),
        );
        catalogue.loaded(vec!["fix".into(), "fix-all".into(), "ns:tool".into()]);
        catalogue
    }

    #[test]
    fn the_last_skill_mention_opens_the_text_and_the_rest_stays_as_typed() {
        let catalogue = catalogue();
        let cases = [
            ("$fix it", Some("/fix it")),
            ("/fix", Some("/fix")),
            ("please $fix this.", Some("/fix please this.")),
            ("run /fix-all, then $fix", Some("/fix run /fix-all, then")),
            ("use $ns:tool now", Some("/ns:tool use now")),
            ("one\n$fix\ntwo", Some("/fix one\ntwo")),
            ("check it, $fix.", Some("/fix check it, .")),
            (
                "/fix-all first, then $fix",
                Some("/fix-all first, then $fix"),
            ),
            ("/compact and $fix", Some("/compact and $fix")),
            ("  /deploy $fix", Some("  /deploy $fix")),
            (
                "keep $deploy, src/fix, /fixer, /fix/a.txt and price$fix",
                None,
            ),
            ("/nope $fix", Some("/fix /nope")),
            ("plain text", None),
        ];
        for (typed, sent) in cases {
            assert_eq!(catalogue.command_text(typed).as_deref(), sent, "{typed}");
        }
    }

    #[test]
    fn the_command_text_goes_after_the_images() {
        let image = InputContent::Image {
            mime_type: "image/png".into(),
            data: "aGk=".into(),
        };
        let text = |text: &str| InputContent::Text { text: text.into() };
        let catalogue = catalogue();
        assert_eq!(
            catalogue.dispatch(&[text("look at $fix"), image.clone()]),
            [image.clone(), text("/fix look at")]
        );
        let untouched = [text("look at this"), image];
        assert_eq!(catalogue.dispatch(&untouched), untouched);
    }

    #[test]
    fn built_ins_leave_the_menu_and_empty_hints_are_none() {
        let mut catalogue = Catalogue::default();
        let mut deploy = command("deploy", false);
        deploy.argument_hint = "[env]".into();
        let menu = catalogue.set_commands(vec![deploy, command("x", false), command("y", true)]);
        let hints: Vec<_> = menu
            .iter()
            .map(|entry| (entry.name.as_str(), entry.argument_hint.as_deref()))
            .collect();
        assert_eq!(hints, [("deploy", Some("[env]")), ("x", None)]);
        assert!(menu.iter().all(|entry| entry.kind == CommandKind::Command));
    }

    #[test]
    fn effort_levels_map_by_name_after_off() {
        let model = wire::Model {
            value: "m".into(),
            supports_effort: true,
            supported_effort_levels: vec!["low".into(), "turbo".into(), "max".into()],
            ..wire::Model::default()
        };
        assert_eq!(
            model_info(&model).thinking_levels,
            [ThinkingLevel::Off, ThinkingLevel::Low, ThinkingLevel::Max]
        );
    }
}
