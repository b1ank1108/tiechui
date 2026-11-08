use tiechui_config::{ModelConfig, ThinkingLevel, ThinkingLevelValue};
use tiechui_model::ReasoningLevel;

use crate::{ErrorKind, ProviderError};

fn value(model: &ModelConfig, level: Option<ReasoningLevel>) -> Option<&ThinkingLevelValue> {
    let key = match level {
        None => ThinkingLevel::Off,
        Some(ReasoningLevel::Minimal) => ThinkingLevel::Minimal,
        Some(ReasoningLevel::Low) => ThinkingLevel::Low,
        Some(ReasoningLevel::Medium) => ThinkingLevel::Medium,
        Some(ReasoningLevel::High) => ThinkingLevel::High,
        Some(ReasoningLevel::Xhigh) => ThinkingLevel::Xhigh,
        Some(ReasoningLevel::Max) => ThinkingLevel::Max,
    };
    model.thinking_levels.as_ref().and_then(|map| map.get(&key))
}

/// Levels of a reasoning model are offered unless marked unsupported, xhigh and
/// max only when mapped; other models only have off.
pub fn offered(model: &ModelConfig) -> Vec<Option<ReasoningLevel>> {
    if !model.reasoning {
        return vec![None];
    }
    std::iter::once(None)
        .chain(ReasoningLevel::ALL.map(Some))
        .filter(|level| match (level, value(model, *level)) {
            (_, Some(ThinkingLevelValue::Unsupported(_))) => false,
            (Some(ReasoningLevel::Xhigh | ReasoningLevel::Max), value) => value.is_some(),
            _ => true,
        })
        .collect()
}

pub(crate) fn mapped(
    model: &ModelConfig,
    level: Option<ReasoningLevel>,
) -> Result<Option<&str>, ProviderError> {
    match value(model, level) {
        Some(ThinkingLevelValue::Wire(value)) => Ok(Some(value)),
        Some(ThinkingLevelValue::Unsupported(_)) if level.is_some() => Err(ProviderError::new(
            ErrorKind::InvalidRequest,
            "requested reasoning level is unsupported by this model",
        )),
        _ => Ok(None),
    }
}
