use std::collections::BTreeSet;

use tiechui_model::ReasoningLevel;

use crate::{model_preset, offered, presets::canonical_preset_ids};

fn check(
    id: &str,
    context_window: u64,
    max_output: u64,
    images: bool,
    expected_levels: &[Option<ReasoningLevel>],
) {
    let preset = model_preset(id).unwrap_or_else(|| panic!("expected preset hit for {id}"));
    assert_eq!(preset.context_window, context_window, "{id} context_window");
    assert_eq!(preset.max_output, max_output, "{id} max_output");
    assert_eq!(preset.images, images, "{id} images");
    assert_eq!(
        preset.reasoning,
        expected_levels != [None],
        "{id} reasoning"
    );
    let config = preset.to_model_config(id);
    assert_eq!(offered(&config), expected_levels, "{id} offered levels");
}

#[test]
fn exact_hits_cover_all_seven_model_families() {
    use ReasoningLevel as R;

    check(
        "claude-opus-5",
        1_000_000,
        128_000,
        true,
        &[
            Some(R::Low),
            Some(R::Medium),
            Some(R::High),
            Some(R::Xhigh),
            Some(R::Max),
        ],
    );
    check(
        "claude-sonnet-4-5",
        200_000,
        64_000,
        true,
        &[
            None,
            Some(R::Low),
            Some(R::Medium),
            Some(R::High),
            Some(R::Max),
        ],
    );
    check(
        "claude-haiku-4-5",
        200_000,
        64_000,
        true,
        &[None, Some(R::Low), Some(R::Medium), Some(R::High)],
    );
    check("claude-3-5-sonnet", 200_000, 8_192, true, &[None]);

    check(
        "gpt-6-astra",
        1_050_000,
        128_000,
        true,
        &[
            Some(R::Low),
            Some(R::Medium),
            Some(R::High),
            Some(R::Xhigh),
            Some(R::Max),
        ],
    );
    check(
        "gpt-5.4",
        1_050_000,
        128_000,
        true,
        &[
            None,
            Some(R::Low),
            Some(R::Medium),
            Some(R::High),
            Some(R::Xhigh),
        ],
    );
    check(
        "gpt-5.3-codex",
        400_000,
        128_000,
        true,
        &[Some(R::Low), Some(R::Medium), Some(R::High), Some(R::Xhigh)],
    );
    check(
        "o3",
        200_000,
        100_000,
        true,
        &[Some(R::Low), Some(R::Medium), Some(R::High)],
    );
    check("gpt-4.1", 1_047_576, 32_768, true, &[None]);
    check("gpt-4o", 128_000, 16_384, true, &[None]);

    check(
        "gemini-4-pro",
        1_048_576,
        65_536,
        true,
        &[
            None,
            Some(R::Minimal),
            Some(R::Low),
            Some(R::Medium),
            Some(R::High),
        ],
    );
    check(
        "gemini-3-flash",
        1_048_576,
        65_536,
        true,
        &[
            None,
            Some(R::Minimal),
            Some(R::Low),
            Some(R::Medium),
            Some(R::High),
        ],
    );
    check(
        "gemini-2.5-pro",
        1_048_576,
        65_536,
        true,
        &[Some(R::Low), Some(R::Medium), Some(R::High)],
    );

    check(
        "deepseek-v4-pro",
        1_000_000,
        384_000,
        false,
        &[None, Some(R::Low), Some(R::High), Some(R::Max)],
    );
    check(
        "deepseek-flash",
        1_000_000,
        384_000,
        true,
        &[None, Some(R::Low), Some(R::High), Some(R::Max)],
    );
    check(
        "deepseek-v3.2",
        128_000,
        64_000,
        false,
        &[None, Some(R::High)],
    );
    check("deepseek-chat", 128_000, 8_192, false, &[None]);
    check(
        "deepseek-reasoner",
        128_000,
        64_000,
        false,
        &[Some(R::High)],
    );

    check(
        "qwen3.8-max",
        1_000_000,
        131_072,
        true,
        &[
            None,
            Some(R::Low),
            Some(R::Medium),
            Some(R::High),
            Some(R::Xhigh),
        ],
    );
    check(
        "qwen3.7-plus",
        1_000_000,
        131_072,
        true,
        &[None, Some(R::Low), Some(R::Medium), Some(R::High)],
    );
    check(
        "qwen3-max",
        262_144,
        65_536,
        false,
        &[None, Some(R::Low), Some(R::Medium), Some(R::High)],
    );
    check(
        "qwen3-vl-plus",
        262_144,
        32_768,
        true,
        &[None, Some(R::Low), Some(R::Medium), Some(R::High)],
    );
    check("qwq-plus", 131_072, 16_384, false, &[Some(R::High)]);

    check(
        "glm-5.3",
        1_000_000,
        128_000,
        false,
        &[None, Some(R::Low), Some(R::High), Some(R::Max)],
    );
    check(
        "glm-5.3-flash",
        1_000_000,
        128_000,
        true,
        &[None, Some(R::Low), Some(R::High), Some(R::Max)],
    );
    check("glm-4.7", 200_000, 131_072, false, &[None, Some(R::High)]);
    check("glm-4.6v", 131_072, 32_768, true, &[None, Some(R::High)]);
    check("glm-4-flash", 131_072, 16_384, false, &[None]);

    check(
        "kimi-k3",
        1_048_576,
        131_072,
        true,
        &[None, Some(R::Low), Some(R::High), Some(R::Max)],
    );
    check(
        "k3-256k",
        262_144,
        131_072,
        true,
        &[None, Some(R::Low), Some(R::High), Some(R::Max)],
    );
    check("kimi-k2.7-code", 262_144, 98_304, true, &[Some(R::High)]);
    check("kimi-k2.5", 262_144, 98_304, true, &[None, Some(R::High)]);
    check("kimi-k2", 262_144, 32_768, false, &[None]);
    check("moonshot-v1-128k", 131_072, 32_000, false, &[None]);
    check(
        "moonshot-v1-128k-vision-preview",
        131_072,
        32_000,
        true,
        &[None],
    );
}

#[test]
fn matches_dated_snapshots_case_dots_and_gateway_prefixes() {
    for (queried, canonical) in [
        ("claude-haiku-4-5-20251001", "claude-haiku-4-5"),
        ("claude-sonnet-4.5-20250929", "claude-sonnet-4-5"),
        ("claude-3-7-sonnet-latest", "claude-3-7-sonnet"),
        ("anthropic/claude-fable-5.1", "claude-fable-5-1"),
        ("gpt-5.4-2026-03-05", "gpt-5.4"),
        ("gpt-4o-2024-11-20", "gpt-4o"),
        ("openai/gpt-5.4", "gpt-5.4"),
        ("gemini-3-flash-preview", "gemini-3-flash"),
        ("gemini-2.5-pro-preview-05-06", "gemini-2.5-pro"),
        ("gemini-2.0-flash-001", "gemini-2.0-flash"),
        ("models/gemini-2.5-flash", "gemini-2.5-flash"),
        ("deepseek/deepseek-v4.1-flash", "deepseek-v4.1-flash"),
        ("qwen/qwen3.8-max-0902", "qwen3.8-max"),
        ("qwen/qwen3.5-plus-20260420", "qwen3.5-plus"),
        ("GLM-4-Flash-250414", "glm-4-flash"),
        ("z-ai/glm-5.3", "glm-5.3"),
        ("moonshotai/kimi-k3", "kimi-k3"),
        ("kimi-k2-0905-preview", "kimi-k2"),
    ] {
        assert_eq!(
            model_preset(queried),
            model_preset(canonical),
            "{queried} should resolve to {canonical}"
        );
        assert!(model_preset(canonical).is_some(), "{canonical} must exist");
    }
    assert!(model_preset("qwen3.7-max-2026-06-08").unwrap().images);
    assert!(!model_preset("qwen3.7-max").unwrap().images);
}

#[test]
fn returns_none_for_unknown_or_ambiguous_ids() {
    for unknown in [
        "",
        "   ",
        "gpt-a",
        "gpt-5.4-custom",
        "claude-unknown-99",
        "gemini-unknown",
        "deepseek-custom",
        "qwen-custom",
        "glm-unknown",
        "kimi-unknown",
        "mock-model",
        "local/unknown-2026-11-15",
        "a/b/gpt-5.4",
    ] {
        assert_eq!(model_preset(unknown), None, "{unknown} must not match");
    }
}

#[test]
fn all_builtin_presets_are_unique_and_well_formed() {
    let ids = canonical_preset_ids();
    assert!(!ids.is_empty());
    let mut seen = BTreeSet::new();
    for id in ids {
        assert!(seen.insert(id), "duplicate preset id {id}");
        let preset = model_preset(id).unwrap_or_else(|| panic!("missing preset {id}"));
        assert!(preset.context_window > 0, "{id} context_window");
        assert!(preset.max_output > 0, "{id} max_output");
        assert!(
            preset.max_output <= preset.context_window,
            "{id} max_output exceeds context_window"
        );
        assert!(
            !offered(&preset.to_model_config(id)).is_empty(),
            "{id} must offer at least one thinking level"
        );
    }
}
