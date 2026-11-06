use crate::{engine::Prepared, messages::check};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::Arc,
};
use tiechui_plugin_api::{CancellationToken, ToolError, model::Role};
use unicode_normalization::UnicodeNormalization;
use unicode_segmentation::UnicodeSegmentation;

fn cjk(c: char) -> bool {
    matches!(c, '\u{3400}'..='\u{9fff}' | '\u{3040}'..='\u{30ff}' | '\u{ac00}'..='\u{d7af}' | '\u{20000}'..='\u{3134f}')
}
fn terms(text: &str) -> Vec<String> {
    let mut split = String::new();
    let mut previous = ' ';
    for c in text.chars() {
        if (previous.is_ascii_lowercase() || previous.is_ascii_digit()) && c.is_ascii_uppercase() {
            split.push(' ');
        }
        split.push(c);
        previous = c;
    }
    let normalized = split.nfkc().collect::<String>().to_lowercase();
    let mut result: Vec<_> = normalized
        .unicode_words()
        .filter(|s| s.chars().any(char::is_alphanumeric))
        .map(str::to_owned)
        .collect();
    let chars: Vec<_> = normalized.chars().collect();
    for pair in chars.windows(2) {
        if pair.iter().all(|c| cjk(*c)) {
            result.push(pair.iter().collect());
        }
    }
    result
}
fn grams(text: &str) -> BTreeSet<String> {
    text.chars()
        .collect::<Vec<_>>()
        .windows(2)
        .map(|v| v.iter().collect())
        .collect()
}
fn similarity(a: &str, b: &str) -> f64 {
    let a = grams(a);
    let b = grams(b);
    2.0 * a.intersection(&b).count() as f64 / (a.len() + b.len()).max(1) as f64
}
type Features = BTreeMap<String, usize>;
#[derive(Default)]
pub struct HistorySearch {
    cache: VecDeque<(String, Arc<Features>, usize)>,
    chars: usize,
}
impl HistorySearch {
    fn features(&mut self, text: &str) -> Arc<Features> {
        let key = crate::messages::hash(text.as_bytes());
        if let Some(index) = self.cache.iter().position(|(hash, _, _)| hash == &key)
            && let Some(entry) = self.cache.remove(index)
        {
            let features = entry.1.clone();
            self.cache.push_back(entry);
            return features;
        }
        let mut counts = BTreeMap::<String, usize>::new();
        for term in terms(text) {
            *counts.entry(term).or_default() += 1;
        }
        let features = Arc::new(counts);
        let chars = text.chars().count();
        if chars <= 128000 {
            while self.cache.len() >= 256 || self.chars + chars > 1000000 {
                if let Some((_, _, size)) = self.cache.pop_front() {
                    self.chars -= size;
                } else {
                    break;
                }
            }
            self.chars += chars;
            self.cache.push_back((key, features.clone(), chars));
        }
        features
    }
    /// Ranked hits over raw branch records and complete summaries; identical texts appear once.
    pub fn search(
        &mut self,
        p: &Prepared,
        query: &str,
        scope: Scope,
        cancel: &CancellationToken,
    ) -> Result<Vec<Hit>, ToolError> {
        let mut documents: Vec<_> = if scope == Scope::Messages {
            vec![]
        } else {
            p.state
                .blocks
                .iter()
                .filter(|b| {
                    b.effective
                        .iter()
                        .all(|id| p.history.parts.iter().any(|m| &m.id == id))
                })
                .map(|b| {
                    (
                        b.id.clone(),
                        format!("{}\n{}", b.topic.as_deref().unwrap_or(""), b.summary),
                        Role::Assistant,
                        "",
                    )
                })
                .collect()
        };
        if scope != Scope::Summaries {
            documents.extend(p.history.parts.iter().map(|p0| {
                (
                    p.state.reference(p0).to_string(),
                    p0.text.clone(),
                    p0.role,
                    p0.tool.as_str(),
                )
            }));
        }
        let mut features = vec![];
        let mut frequency = BTreeMap::<String, usize>::new();
        for (_, text, _, _) in &documents {
            check(cancel)?;
            let counts = self.features(text);
            for word in counts.keys() {
                *frequency.entry(word.clone()).or_default() += 1;
            }
            features.push(counts);
        }
        let terms: BTreeSet<_> = terms(query).into_iter().collect();
        let mut expansions = vec![];
        for term in &terms {
            check(cancel)?;
            let mut candidates: Vec<_> =
                if term.chars().count() >= 4 && !frequency.contains_key(term) {
                    frequency
                        .keys()
                        .filter(|word| {
                            word.chars().count().abs_diff(term.chars().count())
                                <= 2.max(term.chars().count() / 3)
                        })
                        .map(|word| (word.clone(), similarity(term, word) * 0.5))
                        .filter(|(_, weight)| *weight >= 0.32)
                        .collect()
                } else {
                    vec![]
                };
            candidates.sort_by(|a, b| b.1.total_cmp(&a.1));
            candidates.truncate(16);
            candidates.push((term.clone(), 1.0));
            expansions.push(candidates);
        }
        let average = features
            .iter()
            .map(|f| f.values().sum::<usize>())
            .sum::<usize>() as f64
            / features.len().max(1) as f64;
        let mut rows = vec![];
        for (index, ((reference, text, role, tool), feature)) in
            documents.iter().zip(&features).enumerate()
        {
            check(cancel)?;
            let mut score = 0.0;
            let mut matched = 0;
            let length = feature.values().sum::<usize>() as f64;
            for candidates in &expansions {
                let mut best: f64 = 0.0;
                for (word, weight) in candidates {
                    let tf = *feature.get(word).unwrap_or(&0) as f64;
                    if tf == 0.0 {
                        continue;
                    }
                    let df = *frequency.get(word).unwrap_or(&0) as f64;
                    let idf = (1.0 + (documents.len() as f64 - df + 0.5) / (df + 0.5)).ln();
                    best = best.max(
                        weight * idf * tf * 2.2
                            / (tf + 1.2 * (0.25 + 0.75 * length / average.max(1.0))),
                    );
                }
                if best > 0.0 {
                    matched += 1;
                }
                score += best;
            }
            let lower = text.to_lowercase();
            let exact = lower.find(&query.trim().to_lowercase());
            if exact.is_some() {
                score += 1.0;
            }
            score *= (0.5 + matched as f64 / terms.len().max(1) as f64)
                * match role {
                    Role::User => 1.2,
                    Role::Tool => 0.8,
                    _ => 1.0,
                };
            if score <= 0.0 {
                continue;
            }
            let byte = exact
                .or_else(|| terms.iter().find_map(|t| lower.find(t)))
                .unwrap_or(0);
            // Lowercasing can change lengths; map back through the original's characters.
            let position = lower[..byte].chars().count().min(text.chars().count());
            let excerpt = text
                .chars()
                .skip(position.saturating_sub(80))
                .take(320)
                .filter(|c| !c.is_control() || c.is_whitespace())
                .collect::<String>()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            rows.push((
                score,
                index,
                Hit {
                    reference: reference.clone(),
                    summary: reference.starts_with('b'),
                    role: *role,
                    tool: tool.to_string(),
                    position,
                    excerpt,
                    digest: crate::messages::hash(text.as_bytes()),
                },
            ));
        }
        rows.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
        let mut seen = BTreeSet::new();
        Ok(rows
            .into_iter()
            .map(|r| r.2)
            .filter(|hit| seen.insert(hit.digest.clone()))
            .collect())
    }
}
#[derive(Clone, Copy, PartialEq)]
pub enum Scope {
    All,
    Messages,
    Summaries,
}
pub struct Hit {
    pub reference: String,
    pub summary: bool,
    pub role: Role,
    pub tool: String,
    /// Character offset of the match within the record text.
    pub position: usize,
    pub excerpt: String,
    digest: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn feature_cache_is_bounded_and_content_addressed() {
        let mut search = HistorySearch::default();
        for i in 0..300 {
            search.features(&format!("record {i}"));
        }
        assert_eq!(search.cache.len(), 256);
        for i in 0..20 {
            search.features(&format!("record {i} {}", "evidence ".repeat(10000)));
        }
        assert!(search.chars <= 1000000);
        let before = search.chars;
        search.features(&"large ".repeat(30000));
        assert_eq!(search.chars, before);
        assert!(search.features("old record").contains_key("old"));
        assert!(!search.features("new record").contains_key("old"));
    }
}
