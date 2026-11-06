//! Slash commands: the TUI's built-ins and those modules contribute, with the `/` menu.

use ratatui_core::text::Line;
use tiechui_protocol::CommandInfo;

use crate::completion::{self, Row};
use crate::theme::Theme;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Builtin {
    Model,
    Thinking,
    New,
    Resume,
    Reload,
    Help,
    Statusline,
    Theme,
    Trust,
    Quit,
    Retry,
    Compact,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Builtin(Builtin),
    /// Run by the session through `Op::RunCommand`.
    Module,
}

#[derive(Debug, Clone)]
pub struct Command {
    pub info: CommandInfo,
    pub action: Action,
}

fn builtin(name: &str, description: &str, hint: Option<&str>, which: Builtin) -> Command {
    Command {
        info: CommandInfo {
            name: name.into(),
            description: description.into(),
            argument_hint: hint.map(str::to_owned),
        },
        action: Action::Builtin(which),
    }
}

/// Built-ins first; a module command never shadows one.
pub fn catalog(modules: &[CommandInfo]) -> Vec<Command> {
    let mut commands = vec![
        builtin(
            "model",
            "Switch the model",
            Some("provider/model"),
            Builtin::Model,
        ),
        builtin(
            "thinking",
            "Set the thinking level",
            Some("level"),
            Builtin::Thinking,
        ),
        builtin("new", "Start a new session", None, Builtin::New),
        builtin(
            "resume",
            "Resume a different session",
            None,
            Builtin::Resume,
        ),
        builtin(
            "reload",
            "Reload configuration, context files, skills, MCP servers, plugins and hooks",
            None,
            Builtin::Reload,
        ),
        builtin(
            "statusline",
            "Configure the status line",
            None,
            Builtin::Statusline,
        ),
        builtin(
            "trust",
            "Save project trust decision for future sessions",
            Some("allow | deny"),
            Builtin::Trust,
        ),
        builtin("theme", "Select syntax theme", None, Builtin::Theme),
        builtin("help", "Show commands and shortcuts", None, Builtin::Help),
        builtin("quit", "Quit Tiechui", None, Builtin::Quit),
        builtin("retry", "Retry the failed request", None, Builtin::Retry),
        builtin(
            "compact",
            "Compact conversation context",
            Some("instructions"),
            Builtin::Compact,
        ),
    ];
    for info in modules {
        if !commands.iter().any(|c| c.info.name == info.name) {
            commands.push(Command {
                info: info.clone(),
                action: Action::Module,
            });
        }
    }
    commands
}

/// `/name arguments` for a known command; anything else is an ordinary message, so a
/// prompt that starts with a path still reaches the model.
pub fn parse<'a>(commands: &'a [Command], text: &'a str) -> Option<(&'a Command, &'a str)> {
    let rest = text.trim_start().strip_prefix('/')?;
    let (name, arguments) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    commands
        .iter()
        .find(|command| command.info.name == name)
        .map(|command| (command, arguments))
}

/// Prefix matches in catalog order, then other subsequence matches.
pub fn search(commands: &[Command], query: &str) -> Vec<usize> {
    let query = query.to_lowercase();
    let subsequence = |name: &str| {
        let mut rest = name.chars();
        query.chars().all(|c| rest.any(|n| n == c))
    };
    let mut descriptions = Vec::new();
    let (mut prefix, mut other): (Vec<usize>, Vec<usize>) = (Vec::new(), Vec::new());
    for (index, command) in commands.iter().enumerate() {
        let name = command.info.name.to_lowercase();
        if name.starts_with(&query) {
            prefix.push(index);
        } else if subsequence(&name) {
            other.push(index);
        } else if command.info.description.to_lowercase().contains(&query) {
            descriptions.push(index);
        }
    }
    prefix.sort_by_key(|index| commands[*index].info.name.to_lowercase() != query);
    prefix.extend(other);
    prefix.extend(descriptions);
    prefix
}

pub fn popup(
    commands: &[Command],
    matches: &[usize],
    selected: usize,
    query: &str,
    width: usize,
    height: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let rows = matches
        .iter()
        .map(|index| {
            let command = &commands[*index];
            Row {
                label: format!(
                    "/{}{}",
                    command.info.name,
                    command
                        .info
                        .argument_hint
                        .as_ref()
                        .map_or(String::new(), |hint| format!(" [{hint}]"))
                ),
                description: command.info.description.clone(),
                source: match command.action {
                    Action::Builtin(_) => "built-in",
                    Action::Module => "module",
                }
                .into(),
                indices: {
                    let mut query = query.chars().flat_map(char::to_lowercase).peekable();
                    command
                        .info
                        .name
                        .chars()
                        .enumerate()
                        .filter_map(|(at, ch)| {
                            if query
                                .peek()
                                .is_some_and(|q| ch.to_lowercase().any(|c| c == *q))
                            {
                                query.next();
                                Some(at + 1)
                            } else {
                                None
                            }
                        })
                        .collect()
                },
            }
        })
        .collect::<Vec<_>>();
    let status = if rows.is_empty() {
        "No matching commands"
    } else {
        "Tab completes the command; Enter runs it"
    };
    completion::popup(
        "Commands",
        &rows,
        selected,
        status,
        "↑↓ · Tab complete · Enter run · Esc",
        width,
        height,
        theme,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn module(name: &str) -> CommandInfo {
        CommandInfo {
            name: name.into(),
            description: String::new(),
            argument_hint: None,
        }
    }

    #[test]
    fn modules_cannot_shadow_builtins_and_paths_are_not_commands() {
        let commands = catalog(&[module("mcp"), module("quit"), module("status")]);
        assert_eq!(
            commands
                .iter()
                .filter(|c| c.info.name == "quit")
                .map(|c| &c.action)
                .collect::<Vec<_>>(),
            [&Action::Builtin(Builtin::Quit)]
        );
        let (command, arguments) = parse(&commands, " /trust  allow ").unwrap();
        assert_eq!(
            (command.info.name.as_str(), arguments),
            ("trust", " allow ")
        );
        assert_eq!(parse(&commands, "/mcp").unwrap().0.action, Action::Module);
        assert!(parse(&commands, "/tmp/build.log is empty").is_none());
        assert!(parse(&commands, "please /quit").is_none());
        let names = |query| {
            search(&commands, query)
                .into_iter()
                .map(|i| commands[i].info.name.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(names("q")[0], "quit");
        assert_eq!(names("failed request"), ["retry"]);
        assert_eq!(names("s")[0], "statusline");
        assert_eq!(names("status")[0], "status");
        assert!(names("mp").contains(&"mcp".to_owned()));
    }
}
