use std::fs;
use std::io::Write;
use std::path::Path;

use toml_edit::{ArrayOfTables, Document, DocumentMut, InlineTable, Item, Key, Table, Value};

use crate::load::{parse_providers, read_optional};
use crate::{ConfigError, ModelConfig, StatusLineConfig, StatusLineItem, ThinkingLevelValue};

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

/// Removes one dotted TOML key, retaining surrounding comments and unrelated formatting.
pub fn remove_preference(path: &Path, key: &str) -> Result<(), ConfigError> {
    let original = read_optional(path)?;
    let text = original.as_deref().unwrap_or("");
    let mut document: DocumentMut = text
        .parse()
        .map_err(|error: toml_edit::TomlError| ConfigError::new(path, key, error.message()))?;
    let keys = Key::parse(key).map_err(|error| ConfigError::new(path, key, error.message()))?;
    let (last, parents) = keys
        .split_last()
        .ok_or_else(|| ConfigError::new(path, key, "expected a non-empty key"))?;
    let removed = parents
        .iter()
        .try_fold(document.as_item_mut(), |item, key| item.get_mut(key.get()))
        .and_then(Item::as_table_like_mut)
        .and_then(|table| table.remove(last.get()));
    if removed.is_none() {
        return Ok(());
    }
    let rendered = preserve_unchanged_text(text, &document.to_string());
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

/// Sets a provider's wire, address and, when given, inline key in place of `api_key_env`,
/// declaring the provider if it is new. A file holding a key is readable by its owner alone.
pub fn save_provider(
    path: &Path,
    name: &str,
    wire: &str,
    base_url: Option<&str>,
    api_key: Option<&str>,
) -> Result<(), ConfigError> {
    if name.is_empty() || name.contains('/') {
        return Err(ConfigError::new(
            path,
            name,
            "provider names are not empty and have no /",
        ));
    }
    edit_providers(path, name, |document| {
        let fresh = !document.contains_key(name);
        let provider = document
            .entry(name)
            .or_insert(Item::Table(Table::new()))
            .as_table_mut()
            .ok_or("the provider is not a table")?;
        if fresh {
            provider.decor_mut().set_prefix("\n");
        }
        put(provider, "wire", wire.into());
        match base_url {
            Some(url) => put(provider, "base_url", url.into()),
            None => drop(provider.remove("base_url")),
        }
        if let Some(key) = api_key {
            put(provider, "api_key", key.into());
            provider.remove("api_key_env");
        }
        Ok(())
    })?;
    #[cfg(unix)]
    if api_key.is_some() {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|error| ConfigError::new(path, name, error.to_string()))?;
    }
    Ok(())
}

pub fn remove_provider(path: &Path, name: &str) -> Result<(), ConfigError> {
    edit_providers(path, name, |document| {
        document
            .remove(name)
            .map(drop)
            .ok_or("the provider is not declared")
    })
}

/// Writes a model's limits, reasoning, thinking levels and image input, keeping its other keys.
/// `previous` names the model to update; without it the model is added.
pub fn save_model(
    path: &Path,
    provider: &str,
    previous: Option<&str>,
    model: &ModelConfig,
) -> Result<(), ConfigError> {
    edit_providers(path, provider, |document| {
        let models = models(document, provider)?;
        if previous != Some(model.id.as_str()) && position(models, &model.id).is_some() {
            return Err("the model is already declared");
        }
        let table = match previous {
            Some(id) => {
                let index = position(models, id).ok_or("the model is not declared")?;
                models.get_mut(index).ok_or("the model is not declared")?
            }
            None => {
                let mut table = Table::new();
                table.decor_mut().set_prefix("\n");
                models.push(table);
                let last = models.len() - 1;
                models.get_mut(last).ok_or("the model is not declared")?
            }
        };
        let count = |n: u64| {
            i64::try_from(n)
                .map(Value::from)
                .map_err(|_| "a limit is too large")
        };
        put(table, "id", model.id.as_str().into());
        put(table, "context_window", count(model.context_window)?);
        put(table, "max_output", count(model.max_output)?);
        put(table, "reasoning", model.reasoning.into());
        match model
            .thinking_levels
            .as_ref()
            .filter(|levels| !levels.is_empty())
        {
            Some(levels) => {
                let mut inline = InlineTable::new();
                for (level, value) in levels {
                    let name = serde_json::to_value(level)
                        .ok()
                        .and_then(|name| name.as_str().map(str::to_owned))
                        .ok_or("a thinking level has no name")?;
                    let value = match value {
                        ThinkingLevelValue::Wire(wire) => Value::from(wire.as_str()),
                        ThinkingLevelValue::Unsupported(_) => Value::from(false),
                    };
                    inline.insert(&name, value);
                }
                put(table, "thinking_levels", Value::InlineTable(inline));
            }
            None => drop(table.remove("thinking_levels")),
        }
        put(table, "images", model.images.into());
        Ok(())
    })
}

pub fn remove_model(path: &Path, provider: &str, id: &str) -> Result<(), ConfigError> {
    edit_providers(path, provider, |document| {
        let models = models(document, provider)?;
        models.remove(position(models, id).ok_or("the model is not declared")?);
        Ok(())
    })
}

/// Edits `providers.toml` in place and saves it only if every provider still loads.
fn edit_providers(
    path: &Path,
    name: &str,
    edit: impl FnOnce(&mut DocumentMut) -> Result<(), &'static str>,
) -> Result<(), ConfigError> {
    let original = read_optional(path)?;
    let text = original.as_deref().unwrap_or("");
    let mut document = Document::parse(text)
        .map_err(|error| ConfigError::new(path, name, error.message()))?
        .into_mut();
    edit(&mut document).map_err(|message| ConfigError::new(path, name, message))?;
    let rendered = preserve_unchanged_text(text, &document.to_string());
    parse_providers(path, &rendered)?;
    replace(path, name, original, &rendered)
}

/// A provider's models as `[[name.models]]` tables, converting an inline array.
fn models<'a>(
    document: &'a mut DocumentMut,
    provider: &str,
) -> Result<&'a mut ArrayOfTables, &'static str> {
    let provider = document
        .get_mut(provider)
        .and_then(Item::as_table_mut)
        .ok_or("the provider is not declared")?;
    if let Some(inline) = provider.remove("models") {
        let tables = inline
            .into_array_of_tables()
            .map_err(|_| "models are not tables")?;
        provider.insert("models", Item::ArrayOfTables(tables));
    }
    provider
        .entry("models")
        .or_insert(Item::ArrayOfTables(ArrayOfTables::new()))
        .as_array_of_tables_mut()
        .ok_or("models are not tables")
}

fn position(models: &ArrayOfTables, id: &str) -> Option<usize> {
    models
        .iter()
        .position(|model| model.get("id").and_then(Item::as_str) == Some(id))
}

/// Replaces a value, keeping its comments, or adds it.
fn put(table: &mut Table, key: &str, mut value: Value) {
    match table.get_mut(key).and_then(Item::as_value_mut) {
        Some(previous) => {
            *value.decor_mut() = previous.decor().clone();
            *previous = value;
        }
        None => drop(table.insert(key, Item::Value(value))),
    }
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
