use std::collections::BTreeMap;
use tiechui_plugin_api::{CancellationToken, ToolError, serde_json::Value};

use super::Candidate;

fn cjk(c: char) -> bool {
    matches!(c as u32, 0x3400..=0x9fff | 0xf900..=0xfaff | 0x20000..=0x323af | 0x3040..=0x30ff | 0xac00..=0xd7af)
}

fn tokens(text: &str) -> Vec<String> {
    let chars: Vec<_> = text.chars().collect();
    let mut result = Vec::new();
    let mut word = String::new();
    let mut index = 0;
    while index < chars.len() {
        let c = chars[index];
        if cjk(c) {
            if !word.is_empty() {
                result.push(std::mem::take(&mut word));
            }
            let start = index;
            while index < chars.len() && cjk(chars[index]) {
                index += 1;
            }
            if index - start == 1 {
                result.push(c.to_string());
            }
            for pair in chars[start..index].windows(2) {
                result.push(pair.iter().collect());
            }
            continue;
        }
        if c.is_ascii_alphanumeric() {
            if c.is_ascii_uppercase()
                && !word.is_empty()
                && (index > 0 && chars[index - 1].is_ascii_lowercase()
                    || chars.get(index + 1).is_some_and(char::is_ascii_lowercase))
            {
                result.push(std::mem::take(&mut word));
            }
            word.push(c.to_ascii_lowercase());
        } else if !word.is_empty() {
            result.push(std::mem::take(&mut word));
        }
        index += 1;
    }
    if !word.is_empty() {
        result.push(word);
    }
    result
}

fn parameters(schema: &Value, text: &mut String) {
    match schema {
        Value::Object(object) => {
            for (key, value) in object {
                if key == "description"
                    && let Some(description) = value.as_str()
                {
                    text.push(' ');
                    text.push_str(description);
                } else if key == "properties"
                    && let Some(properties) = value.as_object()
                {
                    for (name, schema) in properties {
                        text.push(' ');
                        text.push_str(name);
                        parameters(schema, text);
                    }
                } else {
                    parameters(value, text);
                }
            }
        }
        Value::Array(array) => {
            for value in array {
                parameters(value, text);
            }
        }
        _ => {}
    }
}

pub(super) fn rank(
    candidates: &[Candidate],
    query: &str,
    limit: usize,
    cancel: &CancellationToken,
) -> Result<Vec<String>, ToolError> {
    let query_terms = tokens(query);
    let mut documents = Vec::new();
    let mut frequency = BTreeMap::<String, usize>::new();
    for candidate in candidates {
        if cancel.is_cancelled() {
            return Err(ToolError("Operation aborted".into()));
        }
        let spec = candidate.tool.spec();
        let mut text = format!("{} {}", spec.name, spec.description);
        parameters(&spec.input_schema, &mut text);
        let words = tokens(&text);
        let mut counts = BTreeMap::<String, usize>::new();
        for word in &words {
            *counts.entry(word.clone()).or_default() += 1;
        }
        for term in counts.keys() {
            *frequency.entry(term.clone()).or_default() += 1;
        }
        documents.push((words.len(), counts));
    }
    let average =
        documents.iter().map(|(len, _)| *len).sum::<usize>() as f64 / documents.len().max(1) as f64;
    let mut scores = Vec::new();
    for (candidate, (length, counts)) in candidates.iter().zip(documents) {
        if cancel.is_cancelled() {
            return Err(ToolError("Operation aborted".into()));
        }
        let name = &candidate.tool.spec().name;
        let mut score = if name.eq_ignore_ascii_case(query.trim()) {
            100.0
        } else {
            0.0
        };
        let name_terms = tokens(name);
        for term in &query_terms {
            if name_terms.iter().any(|name| name.starts_with(term)) {
                score += 2.0;
            }
            let count = *counts.get(term).unwrap_or(&0) as f64;
            if count == 0.0 {
                continue;
            }
            let frequency = *frequency.get(term).unwrap_or(&0) as f64;
            let idf = (1.0 + (candidates.len() as f64 - frequency + 0.5) / (frequency + 0.5)).ln();
            score += idf * count * 2.2
                / (count + 1.2 * (0.25 + 0.75 * length as f64 / average.max(1.0)));
        }
        if score > 0.0 {
            scores.push((name.clone(), score));
        }
    }
    scores.sort_by(|(a, x), (b, y)| y.total_cmp(x).then_with(|| a.cmp(b)));
    Ok(scores
        .into_iter()
        .take(limit)
        .map(|(name, _)| name)
        .collect())
}
