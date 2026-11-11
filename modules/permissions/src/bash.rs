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
        let has_env = node
            .named_children(&mut cursor)
            .any(|n| n.kind() == "variable_assignment");
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
        self.words(&words, node_text(node, source), cwd, depth, has_env)
    }

    fn words(
        &mut self,
        words: &[Word],
        typed: &str,
        cwd: &mut PathBuf,
        depth: usize,
        has_env: bool,
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
        let head = command_head(first);
        let values = unit_values(first, head, &words[1..], typed);
        let payload = payload_index(head, words);
        let inline = inline_script_index(head, words);
        let wrapper = is_wrapper(head, payload, inline);
        let argv: Option<Vec<String>> = (!has_env)
            .then(|| words.iter().map(|w| w.value.clone()).collect())
            .flatten();
        let force_read = argv.as_deref().is_some_and(super::readonly::read_only);
        self.program.units.push(Unit {
            values,
            wrapper,
            argv,
        });

        self.expand_wrapper(head, words, inline, payload, cwd, depth)?;
        if skips_outer_operands(head, inline) {
            return Ok(());
        }
        self.extract_operands(head, &words[1..], cwd, force_read)
    }

    fn expand_wrapper(
        &mut self,
        head: &str,
        words: &[Word],
        inline: Option<usize>,
        payload: Option<usize>,
        cwd: &mut PathBuf,
        depth: usize,
    ) -> Result<(), ToolError> {
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
            self.words(&words[at..], &text, cwd, depth + 1, false)?;
        }
        Ok(())
    }

    fn emit_operand(
        &mut self,
        value: &str,
        cwd: &Path,
        direction: &'static str,
        force_read: bool,
    ) -> Result<(), ToolError> {
        if value.is_empty()
            || value.contains(['\n', '\0'])
            || matches!(
                value,
                "-" | "{}" | ";" | "+" | "/dev/null" | "/dev/stdout" | "/dev/stderr" | "/dev/stdin"
            )
        {
            return Ok(());
        }
        let dir = if force_read { "read" } else { direction };
        self.path(value, cwd, dir)
    }

    fn extract_operands(
        &mut self,
        head: &str,
        args: &[Word],
        cwd: &mut PathBuf,
        force_read: bool,
    ) -> Result<(), ToolError> {
        if head == "git" {
            return self.git_operands(args, cwd, force_read);
        }
        for (value, direction) in command_operands(head, args) {
            self.emit_operand(value, cwd, direction, force_read)?;
        }
        if matches!(head, "cd" | "pushd")
            && let Some(path) = positional_args(args, "", &[]).into_iter().flatten().next()
        {
            *cwd = Access::new(path, cwd, self.home, None, "read")?.lexical;
        }
        Ok(())
    }

    fn git_operands(
        &mut self,
        args: &[Word],
        cwd: &Path,
        force_read: bool,
    ) -> Result<(), ToolError> {
        let mut git_cwd = cwd.to_owned();
        let mut at = 0;
        let mut subcommand = None;
        while at < args.len() {
            if let Some((step, dir)) = flag_value(args, at, Some(('C', "Cc")), None) {
                self.apply_git_chdir(dir, &mut git_cwd, force_read)?;
                at += step;
            } else if let Some(step) = git_global_flag_step(&args[at]) {
                at += step;
            } else {
                subcommand = args[at].value.as_deref();
                at += 1;
                break;
            }
        }
        if let Some(sub) = subcommand {
            for (value, direction) in git_subcommand_operands(sub, &args[at..]) {
                self.emit_operand(value, &git_cwd, direction, force_read)?;
            }
        }
        Ok(())
    }

    fn apply_git_chdir(
        &mut self,
        dir: Option<&str>,
        git_cwd: &mut PathBuf,
        force_read: bool,
    ) -> Result<(), ToolError> {
        if let Some(dir) = dir {
            self.emit_operand(dir, git_cwd, "read", force_read)?;
            *git_cwd = Access::new(dir, git_cwd, self.home, None, "read")?.lexical;
        }
        Ok(())
    }
}

fn is_wrapper(head: &str, payload: Option<usize>, inline: Option<usize>) -> bool {
    payload.is_some() || inline.is_some() || head == "eval"
}

fn skips_outer_operands(head: &str, inline: Option<usize>) -> bool {
    matches!(head, "command" | "exec" | "env" | "sudo" | "xargs" | "eval") || inline.is_some()
}

fn git_global_flag_step(word: &Word) -> Option<usize> {
    let token = word.value.as_deref().unwrap_or(&word.raw);
    if matches!(
        token,
        "-c" | "--git-dir" | "--work-tree" | "--namespace" | "--config-env"
    ) {
        Some(2)
    } else if token.starts_with('-') {
        Some(1)
    } else {
        None
    }
}

fn command_head(first: &Word) -> &str {
    let head = first.value.as_deref().unwrap_or(&first.raw);
    Path::new(head)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(head)
}

fn unit_values(first: &Word, head: &str, rest: &[Word], typed: &str) -> Vec<String> {
    let mut values = vec![typed.to_owned()];
    if let Some(value) = &first.value {
        let args = rest
            .iter()
            .map(|w| w.raw.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        values.push(format!("{value} {args}").trim_end().into());
        values.push(format!("{head} {args}").trim_end().into());
    }
    values
}

fn inline_script_index(head: &str, words: &[Word]) -> Option<usize> {
    matches!(head, "sh" | "bash" | "zsh" | "dash" | "ksh" | "fish")
        .then(|| {
            words.iter().position(|w| {
                w.value
                    .as_deref()
                    .is_some_and(|s| s.starts_with('-') && s.contains('c'))
            })
        })
        .flatten()
        .map(|at| at + 1)
}

const SIMPLE_WRITE_SPECS: &[(&[&str], &str, &[&str])] = &[
    (&["touch"], "dtr", &["date", "reference"]),
    (&["mkdir"], "m", &["mode"]),
    (&["rm", "rmdir", "tee"], "", &[]),
    (&["truncate"], "sr", &["size", "reference"]),
];

const SIMPLE_READ_SPECS: &[(&[&str], &str, &[&str])] = &[
    (
        &[
            "cat",
            "realpath",
            "readlink",
            "dirname",
            "md5sum",
            "sha1sum",
            "sha224sum",
            "sha256sum",
            "sha384sum",
            "sha512sum",
            "cd",
            "pushd",
        ],
        "",
        &[],
    ),
    (&["head"], "nc", &["lines", "bytes"]),
    (&["tail"], "ncs", &["lines", "bytes", "sleep-interval"]),
    (&["less"], "hpjP", &["pattern", "tag"]),
    (&["more"], "n", &["lines"]),
    (&["wc"], "", &["files0-from"]),
    (&["stat"], "c", &["format", "printf"]),
    (&["file"], "Fm", &["separator", "magic-file"]),
    (
        &["diff"],
        "LFIxU",
        &[
            "label",
            "ifdef",
            "ignore-matching-lines",
            "exclude",
            "unified",
        ],
    ),
    (
        &["ls"],
        "TIw",
        &["ignore", "hide", "tabsize", "width", "color", "time-style"],
    ),
    (&["strings"], "nte", &["bytes", "radix", "encoding"]),
    (&["hexdump"], "ens", &["length", "skip"]),
    (
        &["od"],
        "AtNjS",
        &[
            "address-radix",
            "format",
            "read-bytes",
            "skip-bytes",
            "strings",
        ],
    ),
    (&["base64"], "w", &["wrap"]),
    (
        &["nl"],
        "swvibhfn",
        &[
            "number-separator",
            "number-width",
            "starting-line-number",
            "line-increment",
            "body-numbering",
            "header-numbering",
            "footer-numbering",
            "number-format",
        ],
    ),
    (&["paste"], "d", &["delimiters"]),
    (
        &["column"],
        "socN",
        &[
            "separator",
            "output-separator",
            "output-width",
            "table-columns",
        ],
    ),
    (
        &["cut"],
        "dfbc",
        &["delimiter", "fields", "bytes", "characters"],
    ),
    (&["basename"], "s", &["suffix"]),
    (&["md5"], "s", &[]),
    (&["shasum"], "a", &["algorithm"]),
];

fn lookup_spec<'a>(
    head: &str,
    table: &'a [(&[&str], &'a str, &'a [&'a str])],
) -> Option<(&'a str, &'a [&'a str])> {
    table
        .iter()
        .find(|(names, _, _)| names.contains(&head))
        .map(|(_, short, long)| (*short, *long))
}

fn command_operands<'a>(head: &str, args: &'a [Word]) -> Vec<(&'a str, &'static str)> {
    if let Some((short, long)) = lookup_spec(head, SIMPLE_WRITE_SPECS) {
        return positional_args(args, short, long)
            .into_iter()
            .flatten()
            .map(|p| (p, "write"))
            .collect();
    }
    if let Some(out) = structured_write_operands(head, args) {
        return out;
    }
    if let Some((short, long)) = lookup_spec(head, SIMPLE_READ_SPECS) {
        return positional_args(args, short, long)
            .into_iter()
            .flatten()
            .map(|p| (p, "read"))
            .collect();
    }
    structured_read_operands(head, args)
}

fn structured_write_operands<'a>(
    head: &str,
    args: &'a [Word],
) -> Option<Vec<(&'a str, &'static str)>> {
    match head {
        "chmod" | "chown" | "chgrp" => Some(chmod_operands(args)),
        "cp" | "mv" | "ln" | "install" => Some(copy_operands(head, args)),
        "dd" => Some(dd_operands(args)),
        "sed" => Some(sed_operands(args)),
        "sort" | "tree" => Some(output_flag_operands(head, args)),
        "find" => Some(find_operands(args)),
        _ => None,
    }
}

fn structured_read_operands<'a>(head: &str, args: &'a [Word]) -> Vec<(&'a str, &'static str)> {
    match head {
        "grep" | "egrep" | "fgrep" | "rg" => grep_operands(args),
        "jq" => jq_operands(args),
        "uniq" => uniq_operands(args),
        _ => Vec::new(),
    }
}

fn is_flag(token: &str, literal: bool) -> bool {
    !literal && token.starts_with('-') && token != "-"
}

fn option_takes_next(token: &str, short_val: &str, long_val: &[&str]) -> bool {
    if let Some(name) = token.strip_prefix("--") {
        return !name.contains('=') && long_val.contains(&name);
    }
    let cluster = token.trim_start_matches('-');
    let Some(pos) = cluster.find(|c| short_val.contains(c)) else {
        return false;
    };
    let len = cluster[pos..].chars().next().map_or(0, char::len_utf8);
    pos + len == cluster.len()
}

fn walk_flags(args: &[Word], mut on_flag: impl FnMut(usize, &str) -> usize) -> Vec<Option<&str>> {
    let mut pos = Vec::new();
    let (mut at, mut literal) = (0, false);
    while let Some(word) = args.get(at) {
        let token = word.value.as_deref().unwrap_or(&word.raw);
        if !literal && token == "--" {
            literal = true;
            at += 1;
        } else if is_flag(token, literal) {
            at += on_flag(at, token);
        } else {
            pos.push(word.value.as_deref());
            at += 1;
        }
    }
    pos
}

fn positional_args<'a>(
    args: &'a [Word],
    short_val: &str,
    long_val: &[&str],
) -> Vec<Option<&'a str>> {
    walk_flags(args, |_, token| {
        if option_takes_next(token, short_val, long_val) {
            2
        } else {
            1
        }
    })
}

fn flag_value<'a>(
    args: &'a [Word],
    at: usize,
    short: Option<(char, &str)>,
    long: Option<&str>,
) -> Option<(usize, Option<&'a str>)> {
    let token = args.get(at)?.value.as_deref()?;
    if token.starts_with("--") {
        return long_flag_value(args, at, token, long?);
    }
    let (ch, stop) = short?;
    short_flag_value(args, at, token, ch, stop)
}

fn long_flag_value<'a>(
    args: &'a [Word],
    at: usize,
    token: &'a str,
    long: &str,
) -> Option<(usize, Option<&'a str>)> {
    let rest = token.strip_prefix("--")?;
    if rest == long {
        return Some((2, args.get(at + 1).and_then(|w| w.value.as_deref())));
    }
    rest.strip_prefix(long)?
        .strip_prefix('=')
        .map(|v| (1, Some(v)))
}

fn short_flag_value<'a>(
    args: &'a [Word],
    at: usize,
    token: &'a str,
    short: char,
    stop: &str,
) -> Option<(usize, Option<&'a str>)> {
    let cluster = token.strip_prefix('-')?;
    let pos = cluster.find(short)?;
    if cluster[..pos].chars().any(|c| stop.contains(c)) {
        return None;
    }
    let tail = &cluster[pos + short.len_utf8()..];
    if tail.is_empty() {
        Some((2, args.get(at + 1).and_then(|w| w.value.as_deref())))
    } else {
        Some((1, Some(tail)))
    }
}

fn has_flag_before_ddash(args: &[Word], target: &str) -> bool {
    args.iter()
        .map(|w| w.value.as_deref().unwrap_or(&w.raw))
        .take_while(|t| *t != "--")
        .any(|t| t == target || t.starts_with(&format!("{target}=")))
}

fn chmod_operands(args: &[Word]) -> Vec<(&str, &'static str)> {
    let skip = usize::from(!has_flag_before_ddash(args, "--reference"));
    positional_args(args, "", &["reference"])
        .into_iter()
        .skip(skip)
        .flatten()
        .map(|p| (p, "write"))
        .collect()
}

struct CopyScan<'a> {
    out: Vec<(&'a str, &'static str)>,
    pos: Vec<Option<&'a str>>,
    has_target: bool,
    dir_mode: bool,
}

fn copy_operands<'a>(head: &str, args: &'a [Word]) -> Vec<(&'a str, &'static str)> {
    let mut scan = scan_copy_args(head, args);
    if scan.has_target {
        scan.out
            .extend(scan.pos.into_iter().flatten().map(|p| (p, "read")));
    } else if scan.dir_mode || scan.pos.len() <= 1 {
        scan.out
            .extend(scan.pos.into_iter().flatten().map(|p| (p, "write")));
    } else {
        let last = scan.pos.len() - 1;
        scan.out.extend(
            scan.pos[..last]
                .iter()
                .copied()
                .flatten()
                .map(|p| (p, "read")),
        );
        if let Some(target) = scan.pos[last] {
            scan.out.push((target, "write"));
        }
    }
    scan.out
}

fn scan_copy_args<'a>(head: &str, args: &'a [Word]) -> CopyScan<'a> {
    let (short_val, stop, long_val): (&str, &str, &[&str]) = if head == "install" {
        ("mogS", "tmogS", &["mode", "owner", "group", "suffix"])
    } else {
        ("S", "tS", &["suffix"])
    };
    let (mut out, mut has_target, mut dir_mode) = (Vec::new(), false, false);
    let pos = walk_flags(args, |at, token| {
        if let Some((step, dir)) = flag_value(args, at, Some(('t', stop)), Some("target-directory"))
        {
            has_target = true;
            if let Some(dir) = dir {
                out.push((dir, "write"));
            }
            return step;
        }
        dir_mode |= head == "install" && copy_install_dir_flag(token);
        if option_takes_next(token, short_val, long_val) {
            2
        } else {
            1
        }
    });
    CopyScan {
        out,
        pos,
        has_target,
        dir_mode,
    }
}

fn copy_install_dir_flag(token: &str) -> bool {
    token == "--directory" || (!token.starts_with("--") && token.contains('d'))
}

fn dd_operands(args: &[Word]) -> Vec<(&str, &'static str)> {
    let mut out = Vec::new();
    for word in args {
        let Some(val) = word.value.as_deref() else {
            continue;
        };
        if let Some(path) = val.strip_prefix("of=") {
            out.push((path, "write"));
        } else if let Some(path) = val.strip_prefix("if=") {
            out.push((path, "read"));
        }
    }
    out
}

fn sed_operands(args: &[Word]) -> Vec<(&str, &'static str)> {
    let (mut out, mut in_place, mut has_script) = (Vec::new(), false, false);
    let pos = walk_flags(args, |at, token| {
        sed_flag(args, at, token, &mut in_place, &mut has_script, &mut out)
    });
    let dir = if in_place { "write" } else { "read" };
    let skip = usize::from(!has_script);
    out.extend(
        pos.into_iter()
            .filter(|v| *v != Some(""))
            .skip(skip)
            .flatten()
            .map(|p| (p, dir)),
    );
    out
}

fn sed_flag<'a>(
    args: &'a [Word],
    at: usize,
    token: &str,
    in_place: &mut bool,
    has_script: &mut bool,
    out: &mut Vec<(&'a str, &'static str)>,
) -> usize {
    if token == "--in-place" || token.starts_with("--in-place=") {
        *in_place = true;
        return 1;
    }
    if let Some((step, _)) = flag_value(args, at, None, Some("expression")) {
        *has_script = true;
        return step;
    }
    if let Some((step, file)) = flag_value(args, at, None, Some("file")) {
        *has_script = true;
        if let Some(file) = file {
            out.push((file, "read"));
        }
        return step;
    }
    if let Some(decoded) = args.get(at).and_then(|w| w.value.as_deref())
        && !decoded.starts_with("--")
        && let Some(cluster) = decoded.strip_prefix('-')
    {
        return sed_short_cluster(args, at, cluster, in_place, has_script, out);
    }
    1
}

fn sed_short_cluster<'a>(
    args: &'a [Word],
    at: usize,
    cluster: &'a str,
    in_place: &mut bool,
    has_script: &mut bool,
    out: &mut Vec<(&'a str, &'static str)>,
) -> usize {
    for (idx, ch) in cluster.char_indices() {
        let tail = &cluster[idx + ch.len_utf8()..];
        match ch {
            'i' => {
                *in_place = true;
                return 1;
            }
            'e' => {
                *has_script = true;
                return if tail.is_empty() { 2 } else { 1 };
            }
            'f' => {
                *has_script = true;
                if let Some(file) = (!tail.is_empty())
                    .then_some(tail)
                    .or_else(|| args.get(at + 1).and_then(|w| w.value.as_deref()))
                {
                    out.push((file, "read"));
                }
                return if tail.is_empty() { 2 } else { 1 };
            }
            _ => {}
        }
    }
    1
}

fn output_flag_operands<'a>(head: &str, args: &'a [Word]) -> Vec<(&'a str, &'static str)> {
    let (short_val, stop, long_val): (&str, &str, &[&str]) = if head == "sort" {
        (
            "ktST",
            "oktST",
            &[
                "key",
                "field-separator",
                "buffer-size",
                "temporary-directory",
            ],
        )
    } else {
        ("LPI", "oLPI", &[])
    };
    let mut out = Vec::new();
    let pos = walk_flags(args, |at, token| {
        if let Some((step, path)) = flag_value(args, at, Some(('o', stop)), Some("output")) {
            if let Some(path) = path {
                out.push((path, "write"));
            }
            return step;
        }
        if option_takes_next(token, short_val, long_val) {
            2
        } else {
            1
        }
    });
    out.extend(pos.into_iter().flatten().map(|p| (p, "read")));
    out
}

fn find_operands(args: &[Word]) -> Vec<(&str, &'static str)> {
    let own = find_own_words(args);
    let mut at = 0;
    while let Some(word) = own.get(at) {
        let token = word.value.as_deref().unwrap_or(&word.raw);
        if token == "-D" {
            at += 2;
        } else if matches!(token, "-H" | "-L" | "-P") || token.starts_with("-O") {
            at += 1;
        } else {
            break;
        }
    }
    let mut roots = Vec::new();
    while let Some(word) = own.get(at) {
        let token = word.value.as_deref().unwrap_or(&word.raw);
        if token.starts_with('-') || matches!(token, "(" | "!") {
            break;
        }
        if let Some(root) = word.value.as_deref() {
            roots.push(root);
        }
        at += 1;
    }
    find_predicate_operands(&own[at..], roots)
}

fn find_own_words(args: &[Word]) -> Vec<&Word> {
    let mut own = Vec::new();
    let mut skip = false;
    for word in args {
        let token = word.value.as_deref().unwrap_or(&word.raw);
        if skip {
            if matches!(token, ";" | "+") {
                skip = false;
            }
        } else if matches!(token, "-exec" | "-execdir" | "-ok" | "-okdir") {
            skip = true;
        } else {
            own.push(word);
        }
    }
    own
}

fn find_predicate_operands<'a>(
    predicates: &[&'a Word],
    roots: Vec<&'a str>,
) -> Vec<(&'a str, &'static str)> {
    let has_delete = predicates
        .iter()
        .any(|w| w.value.as_deref() == Some("-delete"));
    let root_dir = if has_delete { "write" } else { "read" };
    let mut out: Vec<_> = roots.into_iter().map(|r| (r, root_dir)).collect();
    let mut at = 0;
    while let Some(word) = predicates.get(at) {
        if matches!(
            word.value.as_deref(),
            Some("-fprint" | "-fprint0" | "-fls" | "-fprintf")
        ) {
            if let Some(target) = predicates.get(at + 1).and_then(|w| w.value.as_deref()) {
                out.push((target, "write"));
            }
            at += 2;
        } else {
            at += 1;
        }
    }
    out
}

fn git_subcommand_operands<'a>(sub: &str, args: &'a [Word]) -> Vec<(&'a str, &'static str)> {
    let mut out = git_output_flags(args);
    match sub {
        "diff" if has_flag_before_ddash(args, "--no-index") => {
            out.extend(
                positional_args(args, "U", &["unified", "output", "relative"])
                    .into_iter()
                    .take(2)
                    .flatten()
                    .map(|p| (p, "read")),
            );
        }
        "worktree" => {
            let pos = positional_args(args, "bB", &["reason"]);
            if pos.first().copied().flatten() == Some("add")
                && let Some(path) = pos.get(1).copied().flatten()
            {
                out.push((path, "write"));
            }
        }
        "clone" => {
            let pos = positional_args(
                args,
                "boucj",
                &[
                    "branch",
                    "origin",
                    "upload-pack",
                    "reference",
                    "reference-if-able",
                    "separate-git-dir",
                    "config",
                    "depth",
                    "shallow-since",
                    "shallow-exclude",
                    "jobs",
                    "template",
                    "server-option",
                    "filter",
                    "bundle-uri",
                ],
            );
            if let Some(dir) = pos.get(1).copied().flatten() {
                out.push((dir, "write"));
            }
        }
        _ => {}
    }
    out
}

fn git_output_flags(args: &[Word]) -> Vec<(&str, &'static str)> {
    let mut out = Vec::new();
    let mut at = 0;
    while let Some(word) = args.get(at) {
        if word.value.as_deref() == Some("--") {
            break;
        }
        if let Some((step, path)) = flag_value(args, at, None, Some("output")) {
            if let Some(path) = path {
                out.push((path, "write"));
            }
            at += step;
        } else {
            at += 1;
        }
    }
    out
}

fn grep_operands(args: &[Word]) -> Vec<(&str, &'static str)> {
    let (mut out, mut has_pattern) = (Vec::new(), false);
    let pos = walk_flags(args, |at, token| {
        grep_flag(args, at, token, &mut has_pattern, &mut out)
    });
    let skip = usize::from(!has_pattern);
    out.extend(pos.into_iter().skip(skip).flatten().map(|p| (p, "read")));
    out
}

fn grep_flag<'a>(
    args: &'a [Word],
    at: usize,
    token: &str,
    has_pattern: &mut bool,
    out: &mut Vec<(&'a str, &'static str)>,
) -> usize {
    const STOP: &str = "efmABCgtT";
    if let Some((step, _)) = flag_value(args, at, Some(('e', STOP)), Some("regexp")) {
        *has_pattern = true;
        return step;
    }
    if let Some((step, file)) = flag_value(args, at, Some(('f', STOP)), Some("file")) {
        *has_pattern = true;
        if let Some(file) = file {
            out.push((file, "read"));
        }
        return step;
    }
    let takes = option_takes_next(
        token,
        "mABCgtT",
        &[
            "max-count",
            "after-context",
            "before-context",
            "context",
            "glob",
            "type",
            "type-not",
            "max-depth",
            "max-filesize",
            "color",
            "include",
            "exclude",
            "exclude-dir",
        ],
    );
    if takes { 2 } else { 1 }
}

fn jq_operands(args: &[Word]) -> Vec<(&str, &'static str)> {
    let (mut out, mut has_filter) = (Vec::new(), false);
    let pos = walk_flags(args, |at, token| {
        jq_flag(args, at, token, &mut has_filter, &mut out)
    });
    let skip = usize::from(!has_filter);
    out.extend(pos.into_iter().skip(skip).flatten().map(|p| (p, "read")));
    out
}

fn jq_flag<'a>(
    args: &'a [Word],
    at: usize,
    token: &str,
    has_filter: &mut bool,
    out: &mut Vec<(&'a str, &'static str)>,
) -> usize {
    if let Some((step, file)) = flag_value(args, at, Some(('f', "fL")), Some("from-file")) {
        *has_filter = true;
        if let Some(file) = file {
            out.push((file, "read"));
        }
        return step;
    }
    if matches!(token, "--slurpfile" | "--rawfile") {
        if let Some(file) = args.get(at + 2).and_then(|w| w.value.as_deref()) {
            out.push((file, "read"));
        }
        return 3;
    }
    if matches!(token, "--arg" | "--argjson") {
        return 3;
    }
    if option_takes_next(token, "L", &["indent"]) {
        return 2;
    }
    1
}

fn uniq_operands(args: &[Word]) -> Vec<(&str, &'static str)> {
    let pos = positional_args(args, "fsw", &["skip-fields", "skip-chars", "check-chars"]);
    let mut out = Vec::new();
    if let Some(input) = pos.first().copied().flatten() {
        out.push((input, "read"));
    }
    if let Some(output) = pos.get(1).copied().flatten() {
        out.push((output, "write"));
    }
    out
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
