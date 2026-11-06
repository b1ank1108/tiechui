use crate::{
    engine::{Candidate, Prepared, Source},
    messages::tokens,
};
use crate::{messages::maintenance, state::State};
use tiechui_plugin_api::{
    CompactionConfig, PressureBenefit,
    model::{Content, Message, Role},
};

/// Business responses after a maintenance commit before the next unforced one.
pub const COOLDOWN: usize = 3;
/// Business responses still required, while maintenance is cooling down. A new user
/// message ends the cooldown; maintenance-only responses and request-tail hints do not count.
pub fn cooling(state: &State, history: &[Message]) -> Option<usize> {
    let after = history.get(state.maintenance.as_ref()?.at..)?;
    if after.iter().any(|m| m.role == Role::User) {
        return None;
    }
    let business = after
        .iter()
        .filter(|m| m.role == Role::Assistant)
        .filter(|m| {
            let mut calls = m.content.iter().filter_map(|c| match c {
                Content::ToolCall(call) => Some(call.name.as_str()),
                _ => None,
            });
            calls.clone().next().is_none() || calls.any(|name| !maintenance(name))
        })
        .count();
    COOLDOWN.checked_sub(business).filter(|n| *n > 0)
}
#[derive(Default)]
pub struct Cadence {
    pub identity: String,
    pub baseline: u64,
    pub shown: u64,
    pub tiers: [u64; 3],
}
impl Cadence {
    /// Whether to offer candidates now; true selects summary merges over raw history.
    pub fn hint(
        &mut self,
        prepared: &Prepared,
        candidates: &[Candidate],
        config: &CompactionConfig,
        window: u64,
        count: u64,
    ) -> Option<bool> {
        let limit = config.range_window(window);
        let n = &config.nudge;
        let growth = n
            .growth_cap
            .min((limit as f64 * n.growth_ratio).round() as u64)
            .max(1);
        let floor = n
            .min_growth_floor
            .max((growth as f64 * n.min_growth_ratio) as u64);
        let benefit = match n.min_pressure_benefit_tokens {
            PressureBenefit::Tokens(n) => n,
            _ => 5000.max((limit as f64 * 0.01).round() as u64),
        };
        let blocks: Vec<_> = prepared.state.blocks.iter().filter(|b| b.active).collect();
        let pending = [
            candidates
                .iter()
                .filter(|c| matches!(c.source, Source::Messages(_)))
                .map(|c| c.tokens)
                .sum::<u64>(),
            blocks
                .iter()
                .filter(|b| b.tier == 1)
                .map(|b| tokens(&b.summary))
                .sum(),
            blocks
                .iter()
                .filter(|b| b.tier == 2)
                .map(|b| tokens(&b.summary))
                .sum(),
        ];
        let usage = count as f64 / limit as f64;
        let reference = if self.shown > 0 {
            self.shown
        } else if self.baseline > 0 {
            self.baseline
        } else {
            count
        };
        let first = self.shown == 0
            && self.baseline == 0
            && usage >= n.min_context_limit_pct
            && pending.iter().max().copied().unwrap_or(0) >= growth;
        let ready = first || count.saturating_sub(reference) >= floor;
        let pressure = usage >= n.max_context_limit_pct;
        let mut tier = None;
        if pressure {
            let enabled = if config.tiers.enabled { 3 } else { 1 };
            let mut best = 0;
            for (i, p) in pending.iter().enumerate().take(enabled) {
                if *p > best {
                    best = *p;
                    tier = Some(i);
                }
            }
            if best < benefit {
                tier = None;
            }
        } else if ready {
            if pending[0] >= growth {
                tier = Some(0);
            } else if config.tiers.enabled {
                for i in 1..3 {
                    let count_ready = blocks.iter().filter(|b| b.tier as usize == i).count()
                        >= if i == 1 {
                            config.tiers.tier2_trigger
                        } else {
                            config.tiers.tier3_trigger
                        }
                        && usage >= n.min_context_limit_pct;
                    let mass = pending[i]
                        >= (growth as f64 * n.tier2_growth_multiplier).round() as u64
                        && pending[..i].iter().all(|v| pending[i] > *v);
                    if (count_ready || mass)
                        && (self.tiers[i] == 0 || count.saturating_sub(self.tiers[i]) >= floor)
                    {
                        tier = Some(i);
                        break;
                    }
                }
            }
        }
        if self.baseline > count.saturating_add(growth) {
            self.baseline = count;
            self.shown = 0;
            self.tiers = [0; 3];
        }
        if self.baseline == 0 {
            self.baseline = count;
        }
        let tier = tier?;
        self.shown = count;
        self.tiers[tier] = count;
        Some(tier > 0)
    }
}
