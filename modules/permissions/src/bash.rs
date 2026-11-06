use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use tiechui_plugin_api::{CancellationToken, ToolError};
use tree_sitter::{Node, ParseOptions, Parser};

use crate::path::Access;

pub(crate) struct Unit {
    pub values: Vec<String>,
    pub wrapper: bool,
    /// Decoded words; absent when a word is dynamic or variables are assigned inline.
    pub argv: Option<Vec<String>>,
}

#[derive(Default)]
pub(crate) struct Program {
    pub units: Vec<Unit>,
    pub paths: Vec<Access>,
    pub unresolved: bool,
}

pub(crate) fn inspect(
    command: &str,
    cwd: &Path,
    home: &Path,
    cancel: &CancellationToken,
) -> Result<Program, ToolError> {
    let mut inspector = Inspector {
        parser: Parser::new(),
        program: Program::default(),
        home,
        cancel,
        variables: BTreeMap::new(),
        expansions: 0,
    };
    inspector
        .parser
        .set_language(&tree_sitter_bash::LANGUAGE.into())
        .map_err(|e| ToolError(e.to_string()))?;
    inspector.parse(command, cwd, 0)?;
    Ok(inspector.program)
}

struct Inspector<'a> {
    parser: Parser,
    program: Program,
    home: &'a Path,
    cancel: &'a CancellationToken,
    variables: BTreeMap<String, String>,
    expansions: usize,
}

impl Inspector<'_> {
    fn parse(&mut self, source: &str, cwd: &Path, depth: usize) -> Result<(), ToolError> {
        super::check(self.cancel)?;
        self.expansions += 1;
        if depth >= 32 || self.expansions > 256 || source.len() > 256 * 1024 {
            return Err(ToolError(
                "Permission shell inspection limit reached".into(),
            ));
        }
        let mut progress = |_: &tree_sitter::ParseState| self.cancel.is_cancelled();
        let tree = self
            .parser
            .parse_with_options(
                &mut |offset, _| &source.as_bytes()[offset..],
                None,
                Some(ParseOptions::new().progress_callback(&mut progress)),
            )
            .ok_or_else(|| ToolError("Permission shell parser unavailable".into()))?;
        self.program.unresolved |= tree.root_node().has_error();
        let mut cwd = cwd.to_owned();
        self.walk(tree.root_node(), source, &mut cwd, depth)?;
        Ok(())
    }

    fn walk(
        &mut self,
        node: Node<'_>,
        source: &str,
        cwd: &mut PathBuf,
        depth: usize,
    ) -> Result<(), ToolError> {
        super::check(self.cancel)?;
        match node.kind() {
            "variable_assignment" => {
                if let Some((name, value)) = node_text(node, source).split_once('=') {
                    if let Some(value) = decode(value, &self.variables) {
                        self.variables.insert(name.into(), value);
                    } else {
                        self.variables.remove(name);
                    }
                }
            }
            "command" => self.command(node, source, cwd, depth)?,
            "file_redirect" => {
                let text = node_text(node, source);
                let write = text.contains('>');
                let mut cursor = node.walk();
                for child in node.named_children(&mut cursor) {
                    if matches!(
                        child.kind(),
                        "word"
                            | "string"
                            | "raw_string"
                            | "concatenation"
                            | "simple_expansion"
                            | "expansion"
                    ) && let Some(value) = decode(node_text(child, source), &self.variables)
                        && !value.is_empty()
                        && !matches!(
                            value.as_str(),
                            "-" | "/dev/null" | "/dev/stdout" | "/dev/stderr"
                        )
                        && !(text.contains("&") && value.chars().all(|c| c.is_ascii_digit()))
                    {
                        self.path(&value, cwd, if write { "write" } else { "read" })?;
                    }
                }
            }
            _ => {}
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            self.walk(child, source, cwd, depth)?;
        }
        Ok(())
    }

    fn path(&mut self, value: &str, cwd: &Path, direction: &'static str) -> Result<(), ToolError> {
        if self.program.paths.len() >= 8192 {
            return Err(ToolError("Permission shell path limit reached".into()));
        }
        self.program
            .paths
            .push(Access::new(value, cwd, self.home, None, direction)?);
        Ok(())
    }

    fn command(
        &mut self,
        node: Node<'_>,
        source: &str,
        cwd: &mut PathBuf,
        depth: usize,
    ) -> Result<(), ToolError> {
        let mut cursor = node.walk();
        let words: Vec<_> = node
            .named_children(&mut cursor)
            .filter(|n| {
                !matches!(
                    n.kind(),
                    "variable_assignment" | "file_redirect" | "heredoc_redirect"
                )
            })
            .map(|n| Word {
                raw: node_text(n, source).into(),
                value: decode(node_text(n, source), &self.variables),
            })
            .collect();
        if words.is_empty() {
            return Ok(());
        }
        let at = self.program.units.len();
        self.words(&words, node_text(node, source), cwd, depth)?;
        let mut cursor = node.walk();
        if node
            .named_children(&mut cursor)
            .any(|n| n.kind() == "variable_assignment")
            && let Some(unit) = self.program.units.get_mut(at)
        {
            unit.argv = None;
        }
        Ok(())
    }

    fn words(
        &mut self,
        words: &[Word],
        typed: &str,
        cwd: &mut PathBuf,
        depth: usize,
    ) -> Result<(), ToolError> {
        super::check(self.cancel)?;
        if depth >= 32 || self.program.units.len() >= 8192 {
            return Err(ToolError(
                "Permission shell inspection limit reached".into(),
            ));
        }
        let Some(first) = words.first() else {
            return Ok(());
        };
        let head = first.value.as_deref().unwrap_or(&first.raw);
        let head = Path::new(head)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(head);
        let mut values = vec![typed.to_owned()];
        if let Some(value) = &first.value {
            let args = words[1..]
                .iter()
                .map(|w| w.raw.as_str())
                .collect::<Vec<_>>()
                .join(" ");
            values.push(format!("{value} {args}").trim_end().into());
            values.push(format!("{head} {args}").trim_end().into());
        }
        let payload = payload_index(head, words);
        let inline = matches!(head, "sh" | "bash" | "zsh" | "dash" | "ksh" | "fish")
            .then(|| {
                words.iter().position(|w| {
                    w.value
                        .as_deref()
                        .is_some_and(|s| s.starts_with('-') && s.contains('c'))
                })
            })
            .flatten()
            .map(|at| at + 1);
        let wrapper = payload.is_some() || inline.is_some() || head == "eval";
        let argv = words.iter().map(|w| w.value.clone()).collect();
        self.program.units.push(Unit {
            values,
            wrapper,
            argv,
        });

        if head == "eval" {
            let text = words[1..]
                .iter()
                .map(|w| w.value.as_deref().unwrap_or(&w.raw))
                .collect::<Vec<_>>()
                .join(" ");
            self.parse(&text, cwd, depth + 1)?;
        } else if let Some(at) = inline {
            if let Some(Some(text)) = words.get(at).map(|w| &w.value) {
                self.parse(text, cwd, depth + 1)?;
            }
        } else if let Some(at) = payload
            && at < words.len()
        {
            let text = words[at..]
                .iter()
                .map(|w| w.raw.as_str())
                .collect::<Vec<_>>()
                .join(" ");
            self.words(&words[at..], &text, cwd, depth + 1)?;
        }

        if matches!(head, "command" | "exec" | "env" | "sudo" | "xargs" | "eval")
            || inline.is_some()
        {
            return Ok(());
        }
        for word in &words[1..] {
            let Some(value) = word.value.as_deref() else {
                continue;
            };
            if value.is_empty() || value.starts_with('-') || matches!(value, "{}" | ";" | "+") {
                continue;
            }
            // Quoted prose is not a path operand; path-shaped literals still are.
            if matches!(head, "printf" | "echo")
                && !value.starts_with('/')
                && !value.starts_with("../")
                && !value.starts_with("~/")
            {
                continue;
            }
            if matches!(head, "python" | "python3" | "node" | "ruby" | "perl")
                && words
                    .get(1)
                    .is_some_and(|w| matches!(w.value.as_deref(), Some("-c" | "-e")))
            {
                continue;
            }
            if value.contains('\n') || value.contains('\0') {
                continue;
            }
            let direction = match head {
                "cat" | "head" | "tail" | "less" | "more" | "wc" | "ls" | "find" | "grep"
                | "rg" | "cd" | "pushd" => "read",
                "touch" | "mkdir" | "rm" | "rmdir" | "truncate" | "tee" => "write",
                _ => "both",
            };
            self.path(value, cwd, direction)?;
        }
        if matches!(head, "cd" | "pushd")
            && let Some(Some(path)) = words.get(1).map(|w| &w.value)
        {
            *cwd = Access::new(path, cwd, self.home, None, "read")?.lexical;
        }
        Ok(())
    }
}

struct Word {
    raw: String,
    value: Option<String>,
}

fn payload_index(head: &str, words: &[Word]) -> Option<usize> {
    if head == "find" {
        return words
            .iter()
            .position(|w| {
                matches!(
                    w.value.as_deref(),
                    Some("-exec" | "-execdir" | "-ok" | "-okdir")
                )
            })
            .map(|at| at + 1);
    }
    if !matches!(
        head,
        "command"
            | "exec"
            | "env"
            | "sudo"
            | "xargs"
            | "nohup"
            | "nice"
            | "timeout"
            | "stdbuf"
            | "setsid"
            | "time"
    ) {
        return None;
    }
    let mut at = 1;
    while let Some(word) = words.get(at) {
        let value = word.value.as_deref()?;
        if value == "--" {
            return Some(at + 1);
        }
        if head == "env" && value.contains('=') {
            at += 1;
            continue;
        }
        if !value.starts_with('-') || value == "-" {
            if head == "timeout" {
                at += 1;
            }
            return Some(at);
        }
        if head == "command" && value.chars().skip(1).any(|c| c != 'p') {
            return None;
        }
        if head == "exec" {
            let flags = value.trim_start_matches('-');
            for (index, flag) in flags.char_indices() {
                if flag == 'a' {
                    if index + 1 == flags.len() {
                        at += 1;
                    }
                    break;
                }
                if !matches!(flag, 'c' | 'l') {
                    return None;
                }
            }
        } else if matches!(
            (head, value),
            (
                "sudo",
                "-u" | "-g" | "-h" | "-p" | "-C" | "-T" | "-R" | "-D"
            ) | ("env", "-u" | "--unset" | "-C" | "--chdir")
                | ("xargs", "-I" | "-i" | "-n" | "-P" | "-d" | "-s" | "-E")
                | ("nice", "-n")
        ) {
            at += 1;
        }
        at += 1;
    }
    None
}

fn node_text<'a>(node: Node<'_>, source: &'a str) -> &'a str {
    &source[node.byte_range()]
}

/// Decode literal shell words without evaluating substitutions or scripts.
fn decode(text: &str, variables: &BTreeMap<String, String>) -> Option<String> {
    let mut chars = text.chars().peekable();
    let (mut quote, mut result) = (None, String::new());
    while let Some(c) = chars.next() {
        if quote == Some('\'') {
            if c == '\'' {
                quote = None;
            } else {
                result.push(c);
            }
        } else if c == '\\' {
            let next = chars.next()?;
            if quote == Some('"') && !matches!(next, '$' | '`' | '"' | '\\' | '\n') {
                result.push('\\');
            }
            if next != '\n' {
                result.push(next);
            }
        } else if Some(c) == quote {
            quote = None;
        } else if quote.is_none() && matches!(c, '\'' | '"') {
            quote = Some(c);
        } else if c == '`' {
            return None;
        } else if c == '$' {
            if chars.peek() == Some(&'(') {
                return None;
            }
            let braced = chars.peek() == Some(&'{');
            if braced {
                chars.next();
            }
            let mut name = String::new();
            while chars
                .peek()
                .is_some_and(|c| c.is_ascii_alphanumeric() || *c == '_')
            {
                name.push(chars.next()?);
            }
            if braced && chars.next() != Some('}') {
                return None;
            }
            if name.is_empty() {
                return None;
            }
            result.push_str(variables.get(&name)?);
        } else {
            result.push(c);
        }
    }
    quote.is_none().then_some(result)
}
