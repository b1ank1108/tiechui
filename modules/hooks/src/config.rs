use std::collections::BTreeMap;
use std::fs::{self, TryLockError};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tiechui_plugin_api::serde_json::{self, json};
use tiechui_plugin_api::{CancellationToken, HookFile, HookSource, ToolError};

use crate::{Event, check};

#[derive(Deserialize)]
struct HooksFile {
    #[serde(default)]
    hooks: BTreeMap<String, Vec<Group>>,
}

#[derive(Deserialize)]
struct Group {
    matcher: Option<String>,
    #[serde(default)]
    hooks: Vec<CommandHook>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OnFailure {
    #[default]
    Ignore,
    Block,
}

fn is_ignore(mode: &OnFailure) -> bool {
    *mode == OnFailure::Ignore
}

#[derive(Deserialize)]
struct CommandHook {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    command: String,
    #[serde(default)]
    args: Option<Vec<String>>,
    timeout: Option<u64>,
    #[serde(default, rename = "async")]
    asynchronous: bool,
    #[serde(default, rename = "onFailure")]
    on_failure: OnFailure,
    #[serde(rename = "statusMessage")]
    status_message: Option<String>,
    #[serde(rename = "additionalContextLimit")]
    additional_context_limit: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Hook {
    pub key: String,
    pub event: Event,
    pub path: PathBuf,
    pub matcher: Option<String>,
    pub command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,
    pub timeout: u64,
    pub asynchronous: bool,
    #[serde(rename = "onFailure", skip_serializing_if = "is_ignore")]
    pub on_failure: OnFailure,
    pub enabled: bool,
    pub trust: String,
    pub hash: String,
    #[serde(skip)]
    pub(crate) project: bool,
    #[serde(skip)]
    pub(crate) plugin: Option<Arc<dyn HookSource>>,
    #[serde(skip)]
    pointer: Option<String>,
    #[serde(skip)]
    regex: Option<Regex>,
    #[serde(skip)]
    group_index: usize,
    #[serde(skip)]
    handler_index: usize,
}

impl Hook {
    /// `user`, `project` or `plugin`.
    pub fn source(&self) -> &'static str {
        match (&self.plugin, self.project) {
            (Some(_), _) => "plugin",
            (None, true) => "project",
            (None, false) => "user",
        }
    }

    /// Whether [`edit`] may change it: only handlers in `hooks.json` files, whose other content
    /// is hooks alone.
    pub fn editable(&self) -> bool {
        self.plugin.is_none()
            && self.pointer.is_none()
            && self.path.file_name() == Some("hooks.json".as_ref())
    }

    /// What runs, for review: `command` alone for shell handlers, otherwise the program and its
    /// `args` quoted as a POSIX shell would need them.
    pub fn display_command(&self) -> String {
        let Some(args) = &self.args else {
            return self.command.clone();
        };
        let quote = |word: &str| {
            if !word.is_empty()
                && word
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c))
            {
                word.to_owned()
            } else {
                format!("'{}'", word.replace('\'', r"'\''"))
            }
        };
        std::iter::once(self.command.as_str())
            .chain(args.iter().map(String::as_str))
            .map(quote)
            .collect::<Vec<_>>()
            .join(" ")
    }

    pub(crate) fn matches(&self, input: Option<&str>) -> bool {
        if matches!(self.event, Event::UserPromptSubmit | Event::Stop) {
            return true;
        }
        match self.matcher.as_deref() {
            None | Some("" | "*") => true,
            Some(pattern) => input.is_some_and(|input| match &self.regex {
                Some(regex) => regex.is_match(input),
                None => pattern.split('|').any(|candidate| candidate == input),
            }),
        }
    }
}

pub(crate) fn hash(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

fn identity(event: Event, matcher: Option<&str>, command: &CommandHook) -> String {
    let mut handler = serde_json::json!({"type": "command", "command": command.command,
        "timeout": timeout(event, command.timeout), "async": command.asynchronous});
    if let Some(args) = &command.args {
        handler["args"] = serde_json::json!(args);
    }
    if command.on_failure == OnFailure::Block {
        handler["onFailure"] = serde_json::json!("block");
    }
    if let Some(status) = &command.status_message {
        handler["statusMessage"] = serde_json::json!(status);
    }
    if matches!(
        event,
        Event::PreToolUse | Event::PostToolUse | Event::SessionStart | Event::UserPromptSubmit
    ) && let Some(limit) = command
        .additional_context_limit
        .filter(|limit| *limit != 2500)
    {
        handler["additionalContextLimit"] = serde_json::json!(limit);
    }
    let mut identity = serde_json::json!({"event_name": event, "hooks": [handler]});
    if let Some(matcher) = matcher {
        identity["matcher"] = serde_json::json!(matcher);
    }
    // Sorting also keeps hashes stable when a host enables serde_json/preserve_order.
    identity.sort_all_objects();
    hash(identity.to_string().as_bytes())
}

fn timeout(event: Event, value: Option<u64>) -> u64 {
    if event == Event::SessionEnd {
        value.unwrap_or(1).clamp(1, 3)
    } else {
        value.unwrap_or(600).max(1)
    }
}

fn matcher(event: Event, value: Option<&str>) -> Option<&str> {
    if matches!(event, Event::UserPromptSubmit | Event::Stop) {
        None
    } else {
        value
    }
}

pub(crate) fn verify(hook: &Hook, bytes: &[u8]) -> bool {
    let Ok(file) = parse(bytes, hook.pointer.as_deref()) else {
        return false;
    };
    let Some(group) = file
        .hooks
        .get(hook.event.wire())
        .and_then(|groups| groups.get(hook.group_index))
    else {
        return false;
    };
    group.hooks.get(hook.handler_index).is_some_and(|command| {
        command.kind == "command"
            && identity(
                hook.event,
                matcher(hook.event, group.matcher.as_deref()),
                command,
            ) == hook.hash.split('/').next().unwrap_or_default()
    })
}

fn parse(bytes: &[u8], pointer: Option<&str>) -> Result<HooksFile, serde_json::Error> {
    let value: serde_json::Value = serde_json::from_slice(bytes)?;
    serde_json::from_value(match pointer {
        Some(pointer) => value.pointer(pointer).cloned().unwrap_or_default(),
        None => value,
    })
}

pub(crate) fn project_paths(cwd: &Path) -> Vec<PathBuf> {
    let root = cwd
        .ancestors()
        .find(|dir| dir.join(".git").exists())
        .unwrap_or(cwd);
    let mut dirs: Vec<_> = cwd
        .ancestors()
        .take_while(|dir| dir.starts_with(root))
        .collect();
    dirs.reverse();
    dirs.into_iter()
        .flat_map(|dir| {
            [
                dir.join(".codex/hooks.json"),
                dir.join(".agents/hooks.json"),
                dir.join(".claude/settings.json"),
                dir.join(".claude/settings.local.json"),
            ]
        })
        .collect()
}

pub(crate) fn discover(
    cwd: &Path,
    home: &Path,
    trusted: bool,
    sources: &[Arc<dyn HookSource>],
    cancel: &CancellationToken,
) -> Result<(Vec<Hook>, Vec<String>), ToolError> {
    check(cancel)?;
    let state = read_state(home)?;
    let mut hooks = Vec::new();
    let mut diagnostics = Vec::new();
    let user = home.join("hooks.json");
    let files = std::iter::once((user.clone(), false))
        .chain(
            project_paths(cwd)
                .into_iter()
                .filter(|p| p != &user)
                .map(|p| (p, true)),
        )
        .map(|(path, project)| {
            (
                HookFile {
                    path,
                    pointer: None,
                },
                project,
                None,
            )
        });
    let bundled = sources.iter().flat_map(|source| {
        source
            .files()
            .into_iter()
            .map(move |file| (file, false, Some(source.clone())))
    });
    for (file, project, plugin) in files.chain(bundled) {
        let HookFile { path, pointer } = file;
        check(cancel)?;
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                diagnostics.push(format!("{}: {error}", path.display()));
                continue;
            }
        };
        let parsed = match parse(&bytes, pointer.as_deref()) {
            Ok(parsed) => parsed,
            Err(error) => {
                diagnostics.push(format!("{}: {error}", path.display()));
                continue;
            }
        };
        let content_hash = match &plugin {
            Some(source) => match source.content_hash(cancel) {
                Ok(hash) => Some(hash),
                Err(error) => {
                    check(cancel)?;
                    diagnostics.push(format!("{}: {error}", path.display()));
                    continue;
                }
            },
            None => None,
        };
        for (name, groups) in parsed.hooks {
            let Some(event) = Event::from_wire(&name) else {
                diagnostics.push(format!("{}: unsupported event {name}", path.display()));
                continue;
            };
            for (group_index, group) in groups.into_iter().enumerate() {
                let pattern = matcher(event, group.matcher.as_deref());
                let regex = match pattern {
                    Some(pattern)
                        if pattern != "*"
                            && !pattern
                                .chars()
                                .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '|') =>
                    {
                        match Regex::new(pattern) {
                            Ok(regex) => Some(regex),
                            Err(error) => {
                                diagnostics
                                    .push(format!("{}: invalid matcher: {error}", path.display()));
                                continue;
                            }
                        }
                    }
                    _ => None,
                };
                for (handler_index, command) in group.hooks.iter().enumerate() {
                    if command.kind != "command" || command.command.trim().is_empty() {
                        diagnostics.push(format!(
                            "{}: unsupported or empty hook handler",
                            path.display()
                        ));
                        continue;
                    }
                    let declaration_hash = identity(event, pattern, command);
                    let current_hash = content_hash
                        .as_ref()
                        .map_or(declaration_hash.clone(), |hash| {
                            format!("{declaration_hash}/{hash}")
                        });
                    let key = format!(
                        "file:{}:{}:{group_index}:{handler_index}",
                        format_args!(
                            "{}{}",
                            path.display(),
                            pointer
                                .as_ref()
                                .map(|p| format!("#{p}"))
                                .unwrap_or_default()
                        ),
                        serde_json::to_value(event)
                            .map_err(|e| ToolError(e.to_string()))?
                            .as_str()
                            .unwrap_or_default()
                    );
                    let trust = if !project && plugin.is_none() {
                        "user"
                    } else if project && !trusted {
                        "untrusted_project"
                    } else {
                        match state.get(&key) {
                            Some(approved) if approved == &current_hash => "trusted",
                            Some(_) => "changed",
                            None => "unapproved",
                        }
                    };
                    hooks.push(Hook {
                        key,
                        event,
                        path: path.clone(),
                        matcher: pattern.map(str::to_owned),
                        command: command.command.clone(),
                        args: command.args.clone(),
                        timeout: timeout(event, command.timeout),
                        asynchronous: command.asynchronous && event != Event::SessionEnd,
                        on_failure: command.on_failure,
                        enabled: matches!(trust, "trusted" | "user"),
                        trust: trust.into(),
                        hash: current_hash.clone(),
                        project,
                        plugin: plugin.clone(),
                        pointer: pointer.clone(),
                        regex: regex.clone(),
                        group_index,
                        handler_index,
                    });
                }
            }
        }
    }
    Ok((hooks, diagnostics))
}

fn read_state(home: &Path) -> Result<BTreeMap<String, String>, ToolError> {
    match fs::read(home.join("hooks-trust.json")) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|e| ToolError(format!("Invalid hooks trust state: {e}"))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(error) => Err(ToolError(error.to_string())),
    }
}

/// Approves normalized handler identities without granting project trust itself.
pub fn trust(cwd: &Path, home: &Path, cancel: &CancellationToken) -> Result<usize, ToolError> {
    trust_with_sources(cwd, home, true, &[], cancel)
}

pub fn trust_with_sources(
    cwd: &Path,
    home: &Path,
    project_trusted: bool,
    sources: &[Arc<dyn HookSource>],
    cancel: &CancellationToken,
) -> Result<usize, ToolError> {
    update_state(home, cancel, |state| {
        let mut count = 0;
        let cwd = cwd.canonicalize().map_err(|e| ToolError(e.to_string()))?;
        let (hooks, diagnostics) = discover(&cwd, home, project_trusted, sources, cancel)?;
        if !diagnostics.is_empty() {
            return Err(ToolError(diagnostics.join("\n")));
        }
        for hook in hooks
            .into_iter()
            .filter(|hook| (hook.project && project_trusted) || hook.plugin.is_some())
        {
            check(cancel)?;
            state.insert(hook.key, hook.hash);
            count += 1;
        }
        Ok(count)
    })
}

/// A command handler as clients write it.
pub struct Handler {
    /// Saved to the project's `.agents/hooks.json` rather than the user's `hooks.json`.
    pub project: bool,
    pub event: Event,
    pub matcher: Option<String>,
    pub command: String,
    pub timeout: u64,
}

/// Removes the handler named `previous`, adds `handler`, or replaces one with the other,
/// keeping the handler's other fields: in place when the scope, event and matcher stay, moved
/// to a new group otherwise. Files are rewritten with sorted keys. Project handlers approved
/// before stay approved, and the written one is approved: its author has just reviewed it.
pub fn edit(
    cwd: &Path,
    home: &Path,
    previous: Option<&str>,
    handler: Option<Handler>,
    cancel: &CancellationToken,
) -> Result<(), ToolError> {
    let cwd = cwd.canonicalize()?;
    if handler
        .as_ref()
        .is_some_and(|handler| handler.command.trim().is_empty())
    {
        return Err(ToolError("The command is empty".into()));
    }
    update_state(home, cancel, |state| {
        let (hooks, _) = discover(&cwd, home, true, &[], cancel)?;
        let previous = previous
            .map(|key| {
                hooks
                    .iter()
                    .find(|hook| hook.key == key)
                    .ok_or_else(|| ToolError("The hook no longer exists".into()))
            })
            .transpose()?;
        if previous.is_some_and(|hook| !hook.editable()) {
            return Err(ToolError(
                "Only hooks in hooks.json files can be edited".into(),
            ));
        }
        fn pattern(event: Event, value: Option<&str>) -> Option<&str> {
            matcher(event, value).filter(|m| !m.is_empty())
        }
        let in_place = previous.zip(handler.as_ref()).is_some_and(|(hook, new)| {
            hook.project == new.project
                && hook.event == new.event
                && pattern(hook.event, hook.matcher.as_deref())
                    == pattern(new.event, new.matcher.as_deref())
        });
        let mut files = Vec::new();
        let mut moved = None;
        if let Some(hook) = previous {
            let document = document(&mut files, &hook.path)?;
            if let Some(new) = &handler {
                replace(document, hook, new).ok_or_else(|| invalid(&hook.path))?;
            }
            if !in_place {
                moved = Some(remove(document, hook).ok_or_else(|| invalid(&hook.path))?);
            }
        }
        let written = match (handler, previous) {
            (Some(_), Some(hook)) if in_place => Some((
                hook.path.clone(),
                hook.event,
                hook.group_index,
                hook.handler_index,
            )),
            (Some(new), previous) => {
                let path = match previous {
                    Some(hook) if hook.project == new.project => hook.path.clone(),
                    _ if new.project => cwd.join(".agents/hooks.json"),
                    _ => home.join("hooks.json"),
                };
                let handler = moved.unwrap_or_else(|| {
                    json!({"type": "command", "command": new.command.trim(), "timeout": new.timeout})
                });
                let mut group = json!({"hooks": [handler]});
                if let Some(matcher) = pattern(new.event, new.matcher.as_deref()) {
                    group["matcher"] = json!(matcher);
                }
                let index = add(document(&mut files, &path)?, new.event, group)
                    .ok_or_else(|| invalid(&path))?;
                Some((path, new.event, index, 0))
            }
            (None, _) => None,
        };
        let touched = |path: &Path| files.iter().any(|(file, _)| file == path);
        let approved: Vec<_> = hooks
            .iter()
            .filter(|hook| touched(&hook.path) && state.get(&hook.key) == Some(&hook.hash))
            .map(|hook| hook.hash.clone())
            .collect();
        for (path, document) in &files {
            write(path, document)?;
        }
        let (hooks, _) = discover(&cwd, home, true, &[], cancel)?;
        for hook in hooks
            .into_iter()
            .filter(|hook| hook.project && touched(&hook.path))
        {
            let new = written
                .as_ref()
                .is_some_and(|(path, event, group, handler)| {
                    (path, *event, *group, *handler)
                        == (&hook.path, hook.event, hook.group_index, hook.handler_index)
                });
            if new || approved.contains(&hook.hash) {
                state.insert(hook.key, hook.hash);
            }
        }
        Ok(())
    })
}

fn invalid(path: &Path) -> ToolError {
    ToolError(format!("{}: not a hooks file", path.display()))
}

/// The parsed file at `path`, read once; a missing file is empty.
fn document<'a>(
    files: &'a mut Vec<(PathBuf, serde_json::Value)>,
    path: &Path,
) -> Result<&'a mut serde_json::Value, ToolError> {
    let index = match files.iter().position(|(file, _)| file == path) {
        Some(index) => index,
        None => {
            let bytes = match fs::read(path) {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => b"{}".to_vec(),
                Err(error) => return Err(ToolError(format!("{}: {error}", path.display()))),
            };
            let document = parse(&bytes, None)
                .and_then(|_| serde_json::from_slice::<serde_json::Value>(&bytes))
                .ok()
                .filter(serde_json::Value::is_object)
                .ok_or_else(|| invalid(path))?;
            files.push((path.to_owned(), document));
            files.len() - 1
        }
    };
    Ok(&mut files[index].1)
}

fn replace(document: &mut serde_json::Value, hook: &Hook, new: &Handler) -> Option<()> {
    let handler = document
        .get_mut("hooks")?
        .get_mut(hook.event.wire())?
        .get_mut(hook.group_index)?
        .get_mut("hooks")?
        .get_mut(hook.handler_index)?
        .as_object_mut()?;
    handler.insert("command".into(), json!(new.command.trim()));
    handler.insert("timeout".into(), json!(new.timeout));
    Some(())
}

/// Takes the handler out, dropping its group and event when they are left empty.
fn remove(document: &mut serde_json::Value, hook: &Hook) -> Option<serde_json::Value> {
    let events = document.get_mut("hooks")?.as_object_mut()?;
    let groups = events.get_mut(hook.event.wire())?.as_array_mut()?;
    let handlers = groups
        .get_mut(hook.group_index)?
        .get_mut("hooks")?
        .as_array_mut()?;
    if hook.handler_index >= handlers.len() {
        return None;
    }
    let handler = handlers.remove(hook.handler_index);
    if handlers.is_empty() {
        groups.remove(hook.group_index);
        if groups.is_empty() {
            events.remove(hook.event.wire());
        }
    }
    Some(handler)
}

/// Appends a group to the event, returning its index.
fn add(document: &mut serde_json::Value, event: Event, group: serde_json::Value) -> Option<usize> {
    let groups = document
        .as_object_mut()?
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()?
        .entry(event.wire())
        .or_insert_with(|| json!([]))
        .as_array_mut()?;
    groups.push(group);
    Some(groups.len() - 1)
}

fn write(path: &Path, document: &serde_json::Value) -> Result<(), ToolError> {
    let dir = path.parent().ok_or_else(|| invalid(path))?;
    fs::create_dir_all(dir).map_err(|e| ToolError(e.to_string()))?;
    let mut temp = tempfile::NamedTempFile::new_in(dir).map_err(|e| ToolError(e.to_string()))?;
    serde_json::to_writer_pretty(&mut temp, document).map_err(|e| ToolError(e.to_string()))?;
    temp.write_all(b"\n")
        .map_err(|e| ToolError(e.to_string()))?;
    temp.persist(path).map_err(|e| ToolError(e.to_string()))?;
    Ok(())
}

/// Approves exactly these handlers, so a file edited after they were reviewed stays unapproved.
pub fn approve(home: &Path, hooks: &[Hook], cancel: &CancellationToken) -> Result<(), ToolError> {
    update_state(home, cancel, |state| {
        state.extend(
            hooks
                .iter()
                .map(|hook| (hook.key.clone(), hook.hash.clone())),
        );
        Ok(())
    })
}

fn update_state<T>(
    home: &Path,
    cancel: &CancellationToken,
    change: impl FnOnce(&mut BTreeMap<String, String>) -> Result<T, ToolError>,
) -> Result<T, ToolError> {
    check(cancel)?;
    tiechui_plugin_api::private::create_dir_all(home)?;
    let lock = tiechui_plugin_api::private::open_lock(&home.join("hooks-trust.lock"))?;
    loop {
        check(cancel)?;
        match lock.try_lock() {
            Ok(()) => break,
            Err(TryLockError::WouldBlock) => {
                std::thread::sleep(std::time::Duration::from_millis(20))
            }
            Err(error) => return Err(ToolError(error.to_string())),
        }
    }
    let mut state = read_state(home)?;
    let result = change(&mut state)?;
    let mut temp = tempfile::NamedTempFile::new_in(home).map_err(|e| ToolError(e.to_string()))?;
    serde_json::to_writer_pretty(&mut temp, &state).map_err(|e| ToolError(e.to_string()))?;
    temp.write_all(b"\n")
        .map_err(|e| ToolError(e.to_string()))?;
    temp.as_file()
        .sync_all()
        .map_err(|e| ToolError(e.to_string()))?;
    check(cancel)?;
    temp.persist(home.join("hooks-trust.json"))
        .map_err(|e| ToolError(e.to_string()))?;
    Ok(result)
}
