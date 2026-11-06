use crate::messages::Part;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use tiechui_plugin_api::{ToolError, protocol::BranchEntry};

pub const ENTRY: &str = "tiechui.compaction.v1";
/// Version 1 predates candidate-bound blocks; it remains readable.
pub const VERSION: u8 = 2;
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Block {
    pub id: String,
    pub tier: u8,
    pub summary: String,
    pub topic: Option<String>,
    pub direct: Vec<String>,
    pub effective: Vec<String>,
    pub children: Vec<String>,
    pub call: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate: Option<String>,
    pub start: String,
    pub end: String,
    pub active: bool,
    pub tokens: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct State {
    pub version: u8,
    pub blocks: Vec<Block>,
    pub refs: BTreeMap<String, String>,
    pub next_block: u64,
    pub fold: Option<Fold>,
    /// Tool results replaced by previews; recorded so later requests keep the same prefix.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outputs: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maintenance: Option<Maintenance>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Maintenance {
    /// Raw branch messages present when this state was committed; the cooldown counts what follows.
    pub at: usize,
    /// Capacity pressure overrode the cooldown.
    pub forced: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fold {
    pub before: u64,
    pub after: u64,
    pub prefix: u64,
}
impl Default for State {
    fn default() -> Self {
        Self {
            version: VERSION,
            blocks: vec![],
            refs: BTreeMap::new(),
            next_block: 1,
            fold: None,
            outputs: vec![],
            maintenance: None,
        }
    }
}
impl State {
    pub fn read(entries: &[BranchEntry]) -> Result<Self, ToolError> {
        let value = entries
            .iter()
            .rev()
            .filter_map(|e| e.custom.as_ref())
            .find(|(k, _)| k == ENTRY);
        let Some((_, value)) = value else {
            return Ok(Self::default());
        };
        let state: Self = serde_json::from_value(value.clone())
            .map_err(|e| ToolError(format!("Invalid saved compression state: {e}")))?;
        state.validate()?;
        Ok(state)
    }
    pub fn validate(&self) -> Result<(), ToolError> {
        let error = || ToolError("Invalid saved compression references or ancestry".into());
        if !(1..=VERSION).contains(&self.version)
            || !(1..=9007199254740991).contains(&self.next_block)
        {
            return Err(error());
        }
        let mut ids = BTreeSet::new();
        let mut previous = 0;
        for b in &self.blocks {
            let number =
                b.id.strip_prefix('b')
                    .and_then(|s| s.parse::<u64>().ok())
                    .ok_or_else(error)?;
            if number <= previous
                || number >= self.next_block
                || !(1..=3).contains(&b.tier)
                || b.summary.is_empty()
                || b.effective.is_empty()
                || !b.children.iter().all(|id| ids.contains(id))
                || !b.direct.iter().all(|id| b.effective.contains(id))
                || !ids.insert(b.id.clone())
            {
                return Err(error());
            }
            previous = number;
            let mut coverage: BTreeSet<_> = b.direct.iter().collect();
            for child in &b.children {
                let Some(child) = self.block(child) else {
                    return Err(error());
                };
                coverage.extend(child.effective.iter());
            }
            if coverage != b.effective.iter().collect() {
                return Err(error());
            }
            if !b.effective.iter().all(|id| self.refs.contains_key(id)) {
                return Err(error());
            }
        }
        let mut refs = BTreeSet::new();
        for (id, reference) in &self.refs {
            let valid_id = id.strip_prefix("msg-").is_some_and(|s| {
                s.len() > 65
                    && s.as_bytes()[..64].iter().all(|b| b.is_ascii_hexdigit())
                    && s.as_bytes()[64] == b'-'
                    && s.get(65..).is_some_and(|suffix| {
                        let (occurrence, part) = suffix
                            .split_once('/')
                            .map_or((suffix, None), |(a, b)| (a, Some(b)));
                        !occurrence.is_empty()
                            && occurrence.bytes().all(|b| b.is_ascii_digit())
                            && part.is_none_or(|part| {
                                matches!(part, "text" | "thinking")
                                    || part.strip_prefix("call-").is_some_and(|n| {
                                        !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())
                                    })
                            })
                    })
            });
            if !valid_id
                || !reference.starts_with('m')
                || normalize(reference).as_ref() != Some(reference)
                || !refs.insert(reference)
            {
                return Err(error());
            }
        }
        if !self.outputs.iter().all(|id| self.refs.contains_key(id)) {
            return Err(error());
        }
        if self
            .fold
            .as_ref()
            .is_some_and(|f| f.after >= f.before || f.prefix > f.after)
        {
            return Err(error());
        }
        Ok(())
    }
    pub fn assign(&mut self, parts: &[Part]) -> Result<(), ToolError> {
        let mut next = self
            .refs
            .values()
            .filter_map(|r| r[1..].parse::<u32>().ok())
            .max()
            .unwrap_or(0)
            + 1;
        for part in parts {
            if !self.refs.contains_key(&part.id) {
                if next > 99999 {
                    return Err(ToolError("Message reference capacity exhausted".into()));
                }
                self.refs.insert(part.id.clone(), format!("m{next:05}"));
                next += 1;
            }
        }
        Ok(())
    }
    pub fn sync(&mut self, parts: &[Part]) {
        let present: BTreeSet<_> = parts.iter().map(|p| &p.id).collect();
        let children: BTreeSet<_> = self
            .blocks
            .iter()
            .flat_map(|b| b.children.clone())
            .collect();
        for block in &mut self.blocks {
            block.active = !children.contains(&block.id)
                && block.effective.iter().all(|id| present.contains(id));
        }
    }
    pub fn covered(&self) -> BTreeSet<String> {
        self.blocks
            .iter()
            .filter(|b| b.active)
            .flat_map(|b| b.effective.clone())
            .collect()
    }
    pub fn reference(&self, part: &Part) -> &str {
        self.refs.get(&part.id).map_or("BLOCKED", String::as_str)
    }
    pub fn block(&self, id: &str) -> Option<&Block> {
        self.blocks.iter().find(|b| b.id == id)
    }
}
pub fn normalize(reference: &str) -> Option<String> {
    let r = reference.trim().to_ascii_lowercase();
    let n = r.get(1..)?.parse::<u64>().ok()?;
    match r.as_bytes().first()? {
        b'm' if (1..=99999).contains(&n) => Some(format!("m{n:05}")),
        b'b' if n > 0 => Some(format!("b{n}")),
        _ => None,
    }
}
