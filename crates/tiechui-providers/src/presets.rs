use std::collections::BTreeMap;

use tiechui_config::{ModelConfig, ThinkingLevel, ThinkingLevelValue, UnsupportedLevel};

/// Built-in model capabilities and thinking-level overrides keyed by model ID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelPreset {
    pub context_window: u64,
    pub max_output: u64,
    pub reasoning: bool,
    pub thinking_levels: Option<BTreeMap<ThinkingLevel, ThinkingLevelValue>>,
    pub images: bool,
}

impl ModelPreset {
    pub fn to_model_config(&self, id: impl Into<String>) -> ModelConfig {
        ModelConfig {
            id: id.into(),
            cache_retention: Default::default(),
            prices: None,
            fallback_models: Vec::new(),
            context_window: self.context_window,
            max_output: self.max_output,
            reasoning: self.reasoning,
            thinking: Default::default(),
            thinking_levels: self.thinking_levels.clone(),
            compat: None,
            images: self.images,
        }
    }
}

const BASE_LEVELS: [ThinkingLevel; 5] = [
    ThinkingLevel::Off,
    ThinkingLevel::Minimal,
    ThinkingLevel::Low,
    ThinkingLevel::Medium,
    ThinkingLevel::High,
];

struct Spec {
    ids: &'static [&'static str],
    context_window: u64,
    max_output: u64,
    images: bool,
    levels: &'static [ThinkingLevel],
}

use ThinkingLevel as L;

const OFF_ONLY: &[L] = &[L::Off];
const OFF_HIGH: &[L] = &[L::Off, L::High];
const HIGH_ONLY: &[L] = &[L::High];
const LOW_MED_HIGH: &[L] = &[L::Low, L::Medium, L::High];
const MED_HIGH_XHIGH: &[L] = &[L::Medium, L::High, L::Xhigh];
const LOW_MED_HIGH_XHIGH: &[L] = &[L::Low, L::Medium, L::High, L::Xhigh];
const LOW_MED_HIGH_XHIGH_MAX: &[L] = &[L::Low, L::Medium, L::High, L::Xhigh, L::Max];
const OFF_LOW_MED_HIGH: &[L] = &[L::Off, L::Low, L::Medium, L::High];
const OFF_MIN_LOW_MED_HIGH: &[L] = &[L::Off, L::Minimal, L::Low, L::Medium, L::High];
const OFF_LOW_MED_HIGH_MAX: &[L] = &[L::Off, L::Low, L::Medium, L::High, L::Max];
const OFF_LOW_MED_HIGH_XHIGH: &[L] = &[L::Off, L::Low, L::Medium, L::High, L::Xhigh];
const OFF_LOW_MED_HIGH_XHIGH_MAX: &[L] = &[L::Off, L::Low, L::Medium, L::High, L::Xhigh, L::Max];
const OFF_ALL: &[L] = &[
    L::Off,
    L::Minimal,
    L::Low,
    L::Medium,
    L::High,
    L::Xhigh,
    L::Max,
];
const OFF_LOW_HIGH_MAX: &[L] = &[L::Off, L::Low, L::High, L::Max];
const OFF_HIGH_MAX: &[L] = &[L::Off, L::High, L::Max];

const SPECS: &[Spec] = &[
    Spec {
        ids: &[
            "claude-fable-5-2",
            "claude-fable-5-1",
            "claude-fable-5",
            "claude-mythos-5-1",
            "claude-opus-5-5",
            "claude-opus-5",
            "claude-sonnet-5-5",
            "claude-sonnet-5",
        ],
        context_window: 1_000_000,
        max_output: 128_000,
        images: true,
        levels: LOW_MED_HIGH_XHIGH_MAX,
    },
    Spec {
        ids: &["claude-haiku-5-5", "claude-haiku-5"],
        context_window: 1_000_000,
        max_output: 64_000,
        images: true,
        levels: OFF_LOW_MED_HIGH,
    },
    Spec {
        ids: &[
            "claude-opus-4-8",
            "claude-opus-4-7",
            "claude-opus-4-6",
            "claude-opus-4-5",
            "claude-sonnet-4-6",
            "claude-sonnet-4-5",
        ],
        context_window: 200_000,
        max_output: 64_000,
        images: true,
        levels: OFF_LOW_MED_HIGH_MAX,
    },
    Spec {
        ids: &[
            "claude-haiku-4-5",
            "claude-sonnet-4",
            "claude-sonnet-4-0",
            "claude-3-7-sonnet",
        ],
        context_window: 200_000,
        max_output: 64_000,
        images: true,
        levels: OFF_LOW_MED_HIGH,
    },
    Spec {
        ids: &["claude-opus-4-1", "claude-opus-4", "claude-opus-4-0"],
        context_window: 200_000,
        max_output: 32_000,
        images: true,
        levels: OFF_LOW_MED_HIGH,
    },
    Spec {
        ids: &["claude-3-5-sonnet", "claude-3-5-haiku"],
        context_window: 200_000,
        max_output: 8_192,
        images: true,
        levels: OFF_ONLY,
    },
    Spec {
        ids: &["gpt-6-astra", "gpt-6.1-sol", "gpt-6.1-terra", "gpt-6-luna"],
        context_window: 1_050_000,
        max_output: 128_000,
        images: true,
        levels: LOW_MED_HIGH_XHIGH_MAX,
    },
    Spec {
        ids: &["gpt-5.6", "gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna"],
        context_window: 1_050_000,
        max_output: 128_000,
        images: true,
        levels: OFF_LOW_MED_HIGH_XHIGH_MAX,
    },
    Spec {
        ids: &["gpt-5.5", "gpt-5.4"],
        context_window: 1_050_000,
        max_output: 128_000,
        images: true,
        levels: OFF_LOW_MED_HIGH_XHIGH,
    },
    Spec {
        ids: &["gpt-5.5-pro", "gpt-5.4-pro"],
        context_window: 1_050_000,
        max_output: 128_000,
        images: true,
        levels: MED_HIGH_XHIGH,
    },
    Spec {
        ids: &[
            "gpt-5.4-mini",
            "gpt-5.4-nano",
            "gpt-5.2",
            "gpt-5.1",
            "gpt-5",
            "gpt-5.2-codex",
            "gpt-5.1-codex",
            "gpt-5-codex",
        ],
        context_window: 400_000,
        max_output: 128_000,
        images: true,
        levels: OFF_LOW_MED_HIGH_XHIGH,
    },
    Spec {
        ids: &["gpt-5.3-codex", "gpt-5.3-codex-spark"],
        context_window: 400_000,
        max_output: 128_000,
        images: true,
        levels: LOW_MED_HIGH_XHIGH,
    },
    Spec {
        ids: &["gpt-5-mini", "gpt-5-nano"],
        context_window: 400_000,
        max_output: 128_000,
        images: true,
        levels: OFF_MIN_LOW_MED_HIGH,
    },
    Spec {
        ids: &["o3", "o3-pro", "o4-mini", "o1"],
        context_window: 200_000,
        max_output: 100_000,
        images: true,
        levels: LOW_MED_HIGH,
    },
    Spec {
        ids: &["o3-mini"],
        context_window: 200_000,
        max_output: 100_000,
        images: false,
        levels: LOW_MED_HIGH,
    },
    Spec {
        ids: &["gpt-4.1", "gpt-4.1-mini", "gpt-4.1-nano"],
        context_window: 1_047_576,
        max_output: 32_768,
        images: true,
        levels: OFF_ONLY,
    },
    Spec {
        ids: &["gpt-4o", "gpt-4o-mini", "chatgpt-4o"],
        context_window: 128_000,
        max_output: 16_384,
        images: true,
        levels: OFF_ONLY,
    },
    Spec {
        ids: &[
            "gemini-4-pro",
            "gemini-4-flash",
            "gemini-4-carbon",
            "gemini-4-argon",
            "gemini-4-hydrogen",
            "gemini-3.8-flash",
            "gemini-3.7-flash",
            "gemini-3.6-flash",
            "gemini-3.5-pro",
            "gemini-3.5-flash",
            "gemini-3.5-flash-lite",
            "gemini-3.1-pro",
            "gemini-3.1-flash",
            "gemini-3.1-flash-lite",
            "gemini-3-pro",
            "gemini-3-flash",
            "gemini-2.5-flash",
            "gemini-2.5-flash-lite",
        ],
        context_window: 1_048_576,
        max_output: 65_536,
        images: true,
        levels: OFF_MIN_LOW_MED_HIGH,
    },
    Spec {
        ids: &["gemini-2.5-pro"],
        context_window: 1_048_576,
        max_output: 65_536,
        images: true,
        levels: LOW_MED_HIGH,
    },
    Spec {
        ids: &["gemini-2.0-flash", "gemini-2.0-flash-lite"],
        context_window: 1_048_576,
        max_output: 8_192,
        images: true,
        levels: OFF_ONLY,
    },
    Spec {
        ids: &["deepseek-v4-pro", "deepseek-v4", "deepseek-v4-flash"],
        context_window: 1_000_000,
        max_output: 384_000,
        images: false,
        levels: OFF_LOW_HIGH_MAX,
    },
    Spec {
        ids: &[
            "deepseek-flash",
            "deepseek-v4.1-flash",
            "deepseek-v4-1-flash",
            "deepseek-v4-flash-vision",
        ],
        context_window: 1_000_000,
        max_output: 384_000,
        images: true,
        levels: OFF_LOW_HIGH_MAX,
    },
    Spec {
        ids: &["deepseek-v3.2", "deepseek-v3.1"],
        context_window: 128_000,
        max_output: 64_000,
        images: false,
        levels: OFF_HIGH,
    },
    Spec {
        ids: &["deepseek-chat", "deepseek-v3"],
        context_window: 128_000,
        max_output: 8_192,
        images: false,
        levels: OFF_ONLY,
    },
    Spec {
        ids: &["deepseek-reasoner", "deepseek-r1"],
        context_window: 128_000,
        max_output: 64_000,
        images: false,
        levels: HIGH_ONLY,
    },
    Spec {
        ids: &["qwen3.8-max", "qwen3.8-flash"],
        context_window: 1_000_000,
        max_output: 131_072,
        images: true,
        levels: OFF_LOW_MED_HIGH_XHIGH,
    },
    Spec {
        ids: &["qwen3.8-omni-flash"],
        context_window: 65_536,
        max_output: 16_384,
        images: true,
        levels: OFF_ALL,
    },
    Spec {
        ids: &["qwen3.7-max-2026-06-08", "qwen3.7-plus", "qwen3.7-flash"],
        context_window: 1_000_000,
        max_output: 131_072,
        images: true,
        levels: OFF_LOW_MED_HIGH,
    },
    Spec {
        ids: &["qwen3.7-max"],
        context_window: 1_000_000,
        max_output: 131_072,
        images: false,
        levels: OFF_LOW_MED_HIGH,
    },
    Spec {
        ids: &[
            "qwen3.6-plus",
            "qwen3.6-flash",
            "qwen3.5-plus",
            "qwen3.5-flash",
        ],
        context_window: 1_000_000,
        max_output: 65_536,
        images: true,
        levels: OFF_LOW_MED_HIGH,
    },
    Spec {
        ids: &["qwen3-max"],
        context_window: 262_144,
        max_output: 65_536,
        images: false,
        levels: OFF_LOW_MED_HIGH,
    },
    Spec {
        ids: &["qwen3-coder-plus", "qwen-coder-plus"],
        context_window: 1_000_000,
        max_output: 65_536,
        images: false,
        levels: OFF_ONLY,
    },
    Spec {
        ids: &["qwen3-vl-plus", "qwen3-vl-flash"],
        context_window: 262_144,
        max_output: 32_768,
        images: true,
        levels: OFF_LOW_MED_HIGH,
    },
    Spec {
        ids: &["qwen-max"],
        context_window: 131_072,
        max_output: 32_768,
        images: false,
        levels: OFF_ONLY,
    },
    Spec {
        ids: &["qwen-plus", "qwen-flash", "qwen-turbo"],
        context_window: 1_000_000,
        max_output: 32_768,
        images: false,
        levels: OFF_LOW_MED_HIGH,
    },
    Spec {
        ids: &["qwq-plus", "qwq-32b"],
        context_window: 131_072,
        max_output: 16_384,
        images: false,
        levels: HIGH_ONLY,
    },
    Spec {
        ids: &["glm-5.3"],
        context_window: 1_000_000,
        max_output: 128_000,
        images: false,
        levels: OFF_LOW_HIGH_MAX,
    },
    Spec {
        ids: &["glm-5.3-flash"],
        context_window: 1_000_000,
        max_output: 128_000,
        images: true,
        levels: OFF_LOW_HIGH_MAX,
    },
    Spec {
        ids: &["glm-5.2"],
        context_window: 1_000_000,
        max_output: 128_000,
        images: false,
        levels: OFF_HIGH_MAX,
    },
    Spec {
        ids: &["glm-5.1", "glm-5.1-highspeed", "glm-5", "glm-5-turbo"],
        context_window: 200_000,
        max_output: 64_000,
        images: false,
        levels: OFF_HIGH,
    },
    Spec {
        ids: &["glm-5v-turbo"],
        context_window: 200_000,
        max_output: 131_072,
        images: true,
        levels: OFF_HIGH,
    },
    Spec {
        ids: &["glm-4.7", "glm-4.7-flash", "glm-4.7-flashx", "glm-4.6"],
        context_window: 200_000,
        max_output: 131_072,
        images: false,
        levels: OFF_HIGH,
    },
    Spec {
        ids: &["glm-4.6v", "glm-4.6v-flash", "glm-4.6v-flashx"],
        context_window: 131_072,
        max_output: 32_768,
        images: true,
        levels: OFF_HIGH,
    },
    Spec {
        ids: &["glm-4.5", "glm-4.5-air", "glm-4.5-flash"],
        context_window: 131_072,
        max_output: 98_304,
        images: false,
        levels: OFF_HIGH,
    },
    Spec {
        ids: &["glm-4.5v"],
        context_window: 65_536,
        max_output: 16_384,
        images: true,
        levels: OFF_HIGH,
    },
    Spec {
        ids: &["glm-4.1v-thinking-flash", "glm-4.1v-thinking-flashx"],
        context_window: 65_536,
        max_output: 32_768,
        images: true,
        levels: HIGH_ONLY,
    },
    Spec {
        ids: &["glm-4-plus", "glm-4-air", "glm-4-flash", "glm-4-flashx"],
        context_window: 131_072,
        max_output: 16_384,
        images: false,
        levels: OFF_ONLY,
    },
    Spec {
        ids: &["glm-4v-plus", "glm-4v-flash"],
        context_window: 16_384,
        max_output: 4_096,
        images: true,
        levels: OFF_ONLY,
    },
    Spec {
        ids: &[
            "kimi-k3.1-pro",
            "k3.1-pro",
            "kimi-k3.1-pro-highspeed",
            "k3.1-pro-highspeed",
            "kimi-k3",
            "k3",
        ],
        context_window: 1_048_576,
        max_output: 131_072,
        images: true,
        levels: OFF_LOW_HIGH_MAX,
    },
    Spec {
        ids: &["kimi-k3-256k", "k3-256k"],
        context_window: 262_144,
        max_output: 131_072,
        images: true,
        levels: OFF_LOW_HIGH_MAX,
    },
    Spec {
        ids: &["kimi-for-coding", "kimi-for-coding-highspeed"],
        context_window: 262_144,
        max_output: 98_304,
        images: true,
        levels: OFF_LOW_HIGH_MAX,
    },
    Spec {
        ids: &["kimi-k2.7-code", "kimi-k2.7-code-highspeed"],
        context_window: 262_144,
        max_output: 98_304,
        images: true,
        levels: HIGH_ONLY,
    },
    Spec {
        ids: &["kimi-k2.6", "kimi-k2.5"],
        context_window: 262_144,
        max_output: 98_304,
        images: true,
        levels: OFF_HIGH,
    },
    Spec {
        ids: &["kimi-k2", "kimi-k2-turbo", "kimi-k2-instruct"],
        context_window: 262_144,
        max_output: 32_768,
        images: false,
        levels: OFF_ONLY,
    },
    Spec {
        ids: &["kimi-k2-thinking", "kimi-k2-thinking-turbo"],
        context_window: 262_144,
        max_output: 65_536,
        images: false,
        levels: HIGH_ONLY,
    },
    Spec {
        ids: &["kimi-latest"],
        context_window: 131_072,
        max_output: 32_768,
        images: true,
        levels: OFF_ONLY,
    },
    Spec {
        ids: &["moonshot-v1-8k"],
        context_window: 8_192,
        max_output: 8_192,
        images: false,
        levels: OFF_ONLY,
    },
    Spec {
        ids: &["moonshot-v1-32k"],
        context_window: 32_768,
        max_output: 16_384,
        images: false,
        levels: OFF_ONLY,
    },
    Spec {
        ids: &["moonshot-v1-128k", "moonshot-v1-auto"],
        context_window: 131_072,
        max_output: 32_000,
        images: false,
        levels: OFF_ONLY,
    },
    Spec {
        ids: &["moonshot-v1-8k-vision", "moonshot-v1-8k-vision-preview"],
        context_window: 8_192,
        max_output: 8_192,
        images: true,
        levels: OFF_ONLY,
    },
    Spec {
        ids: &["moonshot-v1-32k-vision", "moonshot-v1-32k-vision-preview"],
        context_window: 32_768,
        max_output: 16_384,
        images: true,
        levels: OFF_ONLY,
    },
    Spec {
        ids: &["moonshot-v1-128k-vision", "moonshot-v1-128k-vision-preview"],
        context_window: 131_072,
        max_output: 32_000,
        images: true,
        levels: OFF_ONLY,
    },
];

/// Looks up the built-in parameter preset for `id`, matching canonical names, single-segment
/// gateway prefixes, and standard snapshot suffixes.
pub fn model_preset(id: &str) -> Option<ModelPreset> {
    let lower = id.trim().to_ascii_lowercase();
    if lower.is_empty() || !lower.is_ascii() {
        return None;
    }
    lookup_candidate(&lower).or_else(|| strip_namespace(&lower).and_then(lookup_candidate))
}

fn strip_namespace(id: &str) -> Option<&str> {
    let (ns, rest) = id.split_once('/')?;
    (!ns.is_empty() && !rest.is_empty() && !rest.contains('/')).then_some(rest)
}

fn lookup_candidate(candidate: &str) -> Option<ModelPreset> {
    let mut current = candidate;
    for _ in 0..3 {
        if let Some(preset) = exact_or_claude(current) {
            return Some(preset);
        }
        current = strip_snapshot_suffix(current)?;
    }
    exact_or_claude(current)
}

fn exact_or_claude(id: &str) -> Option<ModelPreset> {
    exact(id).or_else(|| {
        id.starts_with("claude-")
            .then(|| id.replace('.', "-"))
            .and_then(|norm| exact(&norm))
    })
}

fn exact(id: &str) -> Option<ModelPreset> {
    SPECS
        .iter()
        .find(|spec| spec.ids.contains(&id))
        .map(build_preset)
}

fn build_preset(spec: &Spec) -> ModelPreset {
    let reasoning = spec.levels.iter().any(|level| *level != ThinkingLevel::Off);
    let thinking_levels = reasoning.then(|| build_levels(spec.levels)).flatten();
    ModelPreset {
        context_window: spec.context_window,
        max_output: spec.max_output,
        reasoning,
        thinking_levels,
        images: spec.images,
    }
}

fn build_levels(offered: &[ThinkingLevel]) -> Option<BTreeMap<ThinkingLevel, ThinkingLevelValue>> {
    let mut map = BTreeMap::new();
    for level in BASE_LEVELS {
        if !offered.contains(&level) {
            map.insert(level, ThinkingLevelValue::Unsupported(UnsupportedLevel));
        }
    }
    if offered.contains(&ThinkingLevel::Xhigh) {
        map.insert(
            ThinkingLevel::Xhigh,
            ThinkingLevelValue::Wire("xhigh".into()),
        );
    }
    if offered.contains(&ThinkingLevel::Max) {
        map.insert(ThinkingLevel::Max, ThinkingLevelValue::Wire("max".into()));
    }
    (!map.is_empty()).then_some(map)
}

fn strip_snapshot_suffix(id: &str) -> Option<&str> {
    strip_tag_suffix(id)
        .or_else(|| strip_hyphenated_date(id))
        .or_else(|| strip_compact_suffix(id))
}

fn strip_tag_suffix(id: &str) -> Option<&str> {
    ["-latest", "-preview", "-exp"]
        .into_iter()
        .find_map(|tag| id.strip_suffix(tag))
        .filter(|base| !base.is_empty())
}

fn strip_hyphenated_date(id: &str) -> Option<&str> {
    let (before_dd, dd) = id.rsplit_once('-')?;
    let (before_mm, mm) = before_dd.rsplit_once('-')?;
    if !valid_mmdd(mm, dd) {
        return None;
    }
    if is_preview_stem(before_mm) {
        return Some(before_mm);
    }
    let (base, yyyy) = before_mm.rsplit_once('-')?;
    (!base.is_empty() && in_range(yyyy, 4, 2020, 2039)).then_some(base)
}

fn is_preview_stem(stem: &str) -> bool {
    stem.ends_with("-preview") || stem.ends_with("-exp")
}

fn strip_compact_suffix(id: &str) -> Option<&str> {
    let (base, tail) = id.rsplit_once('-')?;
    (!base.is_empty() && tail.bytes().all(|b| b.is_ascii_digit()) && valid_compact_tail(tail))
        .then_some(base)
}

fn valid_compact_tail(tail: &str) -> bool {
    match tail.len() {
        8 => in_range(&tail[0..4], 4, 2020, 2039) && valid_mmdd(&tail[4..6], &tail[6..8]),
        6 => in_range(&tail[0..2], 2, 24, 39) && valid_mmdd(&tail[2..4], &tail[4..6]),
        4 => valid_mmdd(&tail[0..2], &tail[2..4]),
        3 => in_range(tail, 3, 1, 9),
        _ => false,
    }
}

fn valid_mmdd(mm: &str, dd: &str) -> bool {
    in_range(mm, 2, 1, 12) && in_range(dd, 2, 1, 31)
}

fn in_range(s: &str, len: usize, min: u32, max: u32) -> bool {
    s.len() == len
        && s.bytes().all(|b| b.is_ascii_digit())
        && s.parse::<u32>().is_ok_and(|v| (min..=max).contains(&v))
}

#[cfg(test)]
pub(crate) fn canonical_preset_ids() -> Vec<&'static str> {
    SPECS
        .iter()
        .flat_map(|spec| spec.ids.iter().copied())
        .collect()
}
