use std::fs;
use std::io::Write;
use std::path::Path;

use toml_edit::{Document, InlineTable, Item, Key, Table, Value};

use crate::load::read_optional;
use crate::{ConfigError, StatusLineConfig, StatusLineItem};

/// Edit one dotted TOML key, retaining surrounding comments and unrelated formatting.
/// Quoted key segments are supported. Configuration validation happens when loading.
pub fn save_preference(path: &Path, key: &str, mut value: Value) -> Result<(), ConfigError> {
    let original = read_optional(path)?;
    let text = original.as_deref().unwrap_or("");
    let document =
        Document::parse(text).map_err(|error| ConfigError::new(path, key, error.message()))?;
    let keys = Key::parse(key).map_err(|error| ConfigError::new(path, key, error.message()))?;
    let span = keys
        .iter()
        .try_fold(document.as_item(), |item, key| item.get(key.get()))
        .and_then(Item::as_value)
        .and_then(Value::span);
    value.decor_mut().clear();
    let rendered = if let Some(span) = span {
        // Rendering a whole document normalizes newlines, including unrelated CRLF lines.
        let mut rendered = text.to_owned();
        rendered.replace_range(span, &value.to_string());
        rendered
    } else {
        let mut document = document.into_mut();
        set_value(document.as_item_mut(), &keys, value)
            .map_err(|message| ConfigError::new(path, key, message))?;
        preserve_unchanged_text(text, &document.to_string())
    };
    replace(path, key, original, &rendered)
}

/// Appends a `[name]` provider table, rendered as TOML, unless `name` is already declared.
pub fn add_provider(path: &Path, name: &str, table: &str) -> Result<(), ConfigError> {
    let original = read_optional(path)?;
    let text = original.as_deref().unwrap_or("");
    let document =
        Document::parse(text).map_err(|error| ConfigError::new(path, name, error.message()))?;
    if document.contains_key(name) {
        return Err(ConfigError::new(path, name, "provider already exists"));
    }
    let separator = match text {
        "" => "",
        text if text.ends_with('\n') => "\n",
        _ => "\n\n",
    };
    let rendered = format!("{text}{separator}{table}");
    replace(path, name, original, &rendered)
}

/// Replaces `path` atomically unless it changed since `original` was read. New files are 0600.
fn replace(
    path: &Path,
    key: &str,
    original: Option<String>,
    rendered: &str,
) -> Result<(), ConfigError> {
    if original.as_deref() == Some(rendered) {
        return Ok(());
    }
    let target = if original.is_some() {
        fs::canonicalize(path).map_err(|error| ConfigError::new(path, key, error.to_string()))?
    } else {
        path.to_owned()
    };
    let parent = target
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(parent)
        .map_err(|error| ConfigError::new(path, key, error.to_string()))?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .map_err(|error| ConfigError::new(path, key, error.to_string()))?;
    temporary
        .write_all(rendered.as_bytes())
        .map_err(|error| ConfigError::new(path, key, error.to_string()))?;
    if original.is_some() {
        let permissions = fs::metadata(&target)
            .map_err(|error| ConfigError::new(path, key, error.to_string()))?
            .permissions();
        temporary
            .as_file()
            .set_permissions(permissions)
            .map_err(|error| ConfigError::new(path, key, error.to_string()))?;
    }
    temporary
        .as_file()
        .sync_all()
        .map_err(|error| ConfigError::new(path, key, error.to_string()))?;
    if read_optional(path)? != original
        || (original.is_some() && fs::canonicalize(path).ok().as_ref() != Some(&target))
    {
        return Err(ConfigError::new(
            path,
            key,
            "configuration changed while saving; retry the edit",
        ));
    }
    temporary
        .persist(&target)
        .map_err(|error| ConfigError::new(path, key, error.error.to_string()))?;
    Ok(())
}

fn preserve_unchanged_text(original: &str, rendered: &str) -> String {
    let before = original.replace("\r\n", "\n");
    let after = rendered.replace("\r\n", "\n");
    let prefix: usize = before
        .chars()
        .zip(after.chars())
        .take_while(|(left, right)| left == right)
        .map(|(ch, _)| ch.len_utf8())
        .sum();
    let suffix: usize = before[prefix..]
        .chars()
        .rev()
        .zip(after[prefix..].chars().rev())
        .take_while(|(left, right)| left == right)
        .map(|(ch, _)| ch.len_utf8())
        .sum();
    let start = original_offset(original, prefix);
    let end = original_offset(original, before.len() - suffix);
    let inserted = &after[prefix..after.len() - suffix];
    let inserted = if original.contains("\r\n") {
        inserted.replace('\n', "\r\n")
    } else {
        inserted.to_owned()
    };
    format!("{}{}{}", &original[..start], inserted, &original[end..])
}

// Map an offset in newline-normalized text back to the original bytes.
fn original_offset(original: &str, mut offset: usize) -> usize {
    for (index, ch) in original.char_indices() {
        if offset == 0 {
            return index;
        }
        if ch == '\r' && original.as_bytes().get(index + 1) == Some(&b'\n') {
            continue;
        }
        offset -= ch.len_utf8();
    }
    original.len()
}

/// Writes the `[tui.status_line]` keys, each edit keeping the file's formatting.
pub fn save_status_line(path: &Path, line: &StatusLineConfig) -> Result<(), ConfigError> {
    let items =
        |items: &[StatusLineItem]| Value::Array(items.iter().map(|item| item.id()).collect());
    save_preference(path, "tui.status_line.items", items(&line.items))?;
    save_preference(
        path,
        "tui.status_line.use_theme_colors",
        Value::from(line.use_theme_colors),
    )?;
    save_preference(path, "tui.status_line.icons", items(&line.icons))
}

fn set_value(item: &mut Item, keys: &[Key], mut value: Value) -> Result<(), &'static str> {
    let Some((key, remaining)) = keys.split_first() else {
        return Err("expected a non-empty key");
    };
    let inline = item.is_inline_table();
    let table = item
        .as_table_like_mut()
        .ok_or("key crosses a value that is not a table")?;
    let entry = table.entry_format(key).or_insert(Item::None);
    if remaining.is_empty() {
        if let Some(previous) = entry.as_value() {
            value.decor_mut().clone_from(previous.decor());
        } else if !entry.is_none() {
            return Err("key names a table, not a preference value");
        }
        *entry = Item::Value(value);
    } else {
        if entry.is_none() {
            *entry = if inline {
                Item::Value(Value::InlineTable(InlineTable::new()))
            } else {
                let mut table = Table::new();
                table.set_implicit(true);
                Item::Table(table)
            };
        }
        set_value(entry, remaining, value)?;
    }
    Ok(())
}
