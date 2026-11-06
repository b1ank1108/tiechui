use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum PressureBenefit {
    Tokens(#[schemars(range(max = 9007199254740991u64))] u64),
    Automatic(AutoBenefit),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub enum AutoBenefit {
    #[serde(rename = "auto")]
    Auto,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct RangeNudge {
    #[schemars(schema_with = "ratio_schema")]
    pub min_context_limit_pct: f64,
    #[schemars(schema_with = "ratio_schema")]
    pub max_context_limit_pct: f64,
    #[schemars(schema_with = "ratio_schema")]
    pub emergency_threshold_pct: f64,
    #[schemars(schema_with = "ratio_schema")]
    pub growth_ratio: f64,
    #[schemars(range(min = 1, max = 9007199254740991u64))]
    pub growth_cap: u64,
    #[schemars(range(max = 9007199254740991u64))]
    pub min_growth_floor: u64,
    #[schemars(schema_with = "ratio_schema")]
    pub min_growth_ratio: f64,
    #[schemars(range(min = 1, max = 9007199254740991u64))]
    pub tier2_growth_multiplier: f64,
    pub min_pressure_benefit_tokens: PressureBenefit,
}
impl Default for RangeNudge {
    fn default() -> Self {
        Self {
            min_context_limit_pct: 0.45,
            max_context_limit_pct: 0.85,
            emergency_threshold_pct: 0.95,
            growth_ratio: 1.0 / 3.0,
            growth_cap: 100000,
            min_growth_floor: 20000,
            min_growth_ratio: 0.45,
            tier2_growth_multiplier: 1.5,
            min_pressure_benefit_tokens: PressureBenefit::Automatic(AutoBenefit::Auto),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct RangeTiers {
    pub enabled: bool,
    #[schemars(range(min = 1, max = 9007199254740991u64))]
    pub tier2_trigger: usize,
    #[schemars(range(min = 1, max = 9007199254740991u64))]
    pub tier3_trigger: usize,
}
impl Default for RangeTiers {
    fn default() -> Self {
        Self {
            enabled: true,
            tier2_trigger: 5,
            tier3_trigger: 10,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct RangeCompress {
    #[schemars(range(max = 9007199254740991u64))]
    pub min_compress_range: usize,
    #[schemars(range(min = 1, max = 9007199254740991u64))]
    pub min_summary_length: usize,
    #[schemars(range(min = 1, max = 9007199254740991u64))]
    pub max_summary_length: usize,
    #[schemars(range(max = 9007199254740991u64))]
    pub min_net_savings_tokens: u64,
}
impl Default for RangeCompress {
    fn default() -> Self {
        Self {
            min_compress_range: 5000,
            min_summary_length: 50,
            max_summary_length: 20000,
            min_net_savings_tokens: 1000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct RangeSearch {
    #[schemars(range(min = 1, max = 9007199254740991u64))]
    pub default_limit: usize,
    #[schemars(range(min = 1, max = 9007199254740991u64))]
    pub max_limit: usize,
}
impl Default for RangeSearch {
    fn default() -> Self {
        Self {
            default_limit: 10,
            max_limit: 30,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct RangeOutput {
    pub enabled: bool,
    #[schemars(range(min = 2000, max = 9007199254740991u64))]
    pub min_chars: usize,
}
impl Default for RangeOutput {
    fn default() -> Self {
        Self {
            enabled: true,
            min_chars: 20000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[derive(Default)]
pub struct RangeDecompress {
    pub full: bool,
}

impl crate::CompactionConfig {
    pub fn validate_range(&self) -> Result<(), String> {
        let n = &self.nudge;
        if [
            n.min_context_limit_pct,
            n.max_context_limit_pct,
            n.emergency_threshold_pct,
            n.growth_ratio,
            n.min_growth_ratio,
        ]
        .iter()
        .any(|v| !v.is_finite() || *v <= 0.0 || *v > 1.0)
        {
            return Err("compaction ratios must be finite and in (0, 1]".into());
        }
        if n.min_context_limit_pct > n.max_context_limit_pct
            || n.max_context_limit_pct > n.emergency_threshold_pct
        {
            return Err("compaction requires min_context_limit_pct <= max_context_limit_pct <= emergency_threshold_pct".into());
        }
        if !n.tier2_growth_multiplier.is_finite()
            || !(1.0..=9007199254740991.0).contains(&n.tier2_growth_multiplier)
        {
            return Err("compaction tier2_growth_multiplier must be finite and >= 1".into());
        }
        if self.context_window_cap == 0
            || self.fallback_context_window == 0
            || n.growth_cap == 0
            || self.tiers.tier2_trigger == 0
            || self.tiers.tier3_trigger == 0
            || self.compress.min_summary_length == 0
            || self.compress.min_summary_length > self.compress.max_summary_length
            || self.search.default_limit == 0
            || self.search.default_limit > self.search.max_limit
            || self.output.min_chars < 2000
        {
            return Err("Invalid compaction limits".into());
        }
        let mut integers = vec![
            self.context_window_cap,
            self.fallback_context_window,
            self.preserve_recent_messages as u64,
            self.preserve_recent_tokens,
            n.growth_cap,
            n.min_growth_floor,
            self.tiers.tier2_trigger as u64,
            self.tiers.tier3_trigger as u64,
            self.compress.min_compress_range as u64,
            self.compress.min_summary_length as u64,
            self.compress.max_summary_length as u64,
            self.compress.min_net_savings_tokens,
            self.search.default_limit as u64,
            self.search.max_limit as u64,
            self.output.min_chars as u64,
        ];
        if let PressureBenefit::Tokens(v) = n.min_pressure_benefit_tokens {
            integers.push(v);
        }
        if integers.iter().any(|v| *v > 9007199254740991) {
            return Err("Compaction integers must be safe integers".into());
        }
        if self.protected_tools.iter().any(|v| {
            v.is_empty()
                || v.chars().any(char::is_whitespace)
                || v.strip_suffix('*').unwrap_or(v).contains('*')
        }) {
            return Err("protected_tools requires names or trailing-* patterns".into());
        }
        Ok(())
    }
    pub fn range_window(&self, window: u64) -> u64 {
        self.context_window_cap.min(if window == 0 {
            self.fallback_context_window
        } else {
            window
        })
    }
}

fn ratio_schema(_: &mut schemars::r#gen::SchemaGenerator) -> schemars::schema::Schema {
    use schemars::schema::{InstanceType, NumberValidation, SchemaObject};
    SchemaObject {
        instance_type: Some(InstanceType::Number.into()),
        number: Some(Box::new(NumberValidation {
            exclusive_minimum: Some(0.0),
            maximum: Some(1.0),
            ..Default::default()
        })),
        ..Default::default()
    }
    .into()
}
