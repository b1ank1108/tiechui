use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    time::{Instant, SystemTime, UNIX_EPOCH},
};
use tiechui_plugin_api::{ToolError, model::Usage};

pub const ENTRY: &str = "tiechui.goal.state";
pub const STATUS: &str = "tiechui.goal.status";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Active,
    Paused,
    Blocked,
    UsageLimited,
    Complete,
}
impl Status {
    pub fn name(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Paused => "paused",
            Self::Blocked => "blocked",
            Self::UsageLimited => "usage_limited",
            Self::Complete => "complete",
        }
    }
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Goal {
    pub id: String,
    pub objective: String,
    pub status: Status,
    pub tokens_used: u64,
    pub time_used_seconds: f64,
    pub usage_complete: bool,
    pub created_at: String,
    pub updated_at: String,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct State {
    pub version: u8,
    pub goal: Option<Goal>,
    pub deferred: bool,
}
impl Default for State {
    fn default() -> Self {
        Self {
            version: 1,
            goal: None,
            deferred: false,
        }
    }
}
impl State {
    pub fn read(value: serde_json::Value) -> Option<Self> {
        value.get("goal")?;
        let state: Self = serde_json::from_value(value).ok()?;
        if state.version != 1 {
            return None;
        }
        if let Some(g) = &state.goal
            && (g.id.is_empty()
                || g.objective.trim().is_empty()
                || !g.time_used_seconds.is_finite()
                || g.time_used_seconds < 0.0
                || !valid_date(&g.created_at)
                || !valid_date(&g.updated_at))
        {
            return None;
        }
        Some(state)
    }
}

pub struct Run {
    pub goal_id: Option<String>,
    pub automatic: bool,
    pub tools: bool,
    pub clock: Option<Instant>,
    pub messages: HashSet<String>,
    pub error: Option<String>,
}
#[derive(Default)]
pub struct Service {
    pub state: State,
    pub revision: u64,
    pub run: Option<Run>,
    pub wanted: bool,
    pub storage_failed: bool,
    pub rewind: bool,
    pub seen: bool,
    pub session: String,
    /// Another module's automatic turn is running; it never touches the goal.
    pub foreign: bool,
}
impl Service {
    pub fn active(&self) -> bool {
        self.state
            .goal
            .as_ref()
            .is_some_and(|g| g.status == Status::Active)
    }
    pub fn snapshot(&self) -> State {
        let mut state = self.state.clone();
        if let (Some(goal), Some(run)) = (&mut state.goal, &self.run)
            && run.goal_id.as_ref() == Some(&goal.id)
            && let Some(clock) = run.clock
        {
            goal.time_used_seconds += clock.elapsed().as_secs_f64();
        }
        state
    }
    pub fn flush(&mut self) {
        self.state = self.snapshot();
        if let Some(run) = &mut self.run
            && run.clock.is_some()
        {
            run.clock = Some(Instant::now());
        }
    }
    pub fn changed(&mut self) {
        self.seen = true;
        self.revision = self.revision.wrapping_add(1);
        if let Some(g) = &mut self.state.goal {
            g.updated_at = timestamp();
        }
    }
    pub fn create(&mut self, objective: &str, replace: bool, bind: bool) -> Result<(), ToolError> {
        let objective = objective.trim();
        if objective.is_empty() {
            return Err(ToolError("A goal needs a non-empty objective.".into()));
        }
        if !replace
            && self
                .state
                .goal
                .as_ref()
                .is_some_and(|g| g.status != Status::Complete)
        {
            return Err(ToolError(
                "An unfinished goal exists. Use /goal edit, pause, resume or clear.".into(),
            ));
        }
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let id = format!(
            "goal-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let now = timestamp();
        self.state = State {
            version: 1,
            deferred: false,
            goal: Some(Goal {
                id: id.clone(),
                objective: objective.into(),
                status: Status::Active,
                tokens_used: 0,
                time_used_seconds: 0.0,
                usage_complete: true,
                created_at: now.clone(),
                updated_at: now,
            }),
        };
        if bind
            && let Some(run) = &mut self.run
            && !run.automatic
        {
            run.goal_id = Some(id);
            run.clock = Some(Instant::now());
            run.messages.clear();
        }
        self.changed();
        Ok(())
    }
    pub fn edit(&mut self, objective: &str) -> Result<(), ToolError> {
        if objective.trim().is_empty() {
            return Err(ToolError("A goal needs a non-empty objective.".into()));
        }
        self.flush();
        let goal = self
            .state
            .goal
            .as_mut()
            .ok_or_else(|| ToolError("No goal to edit.".into()))?;
        goal.objective = objective.trim().into();
        if goal.status == Status::Complete {
            goal.status = Status::Active;
        }
        self.changed();
        Ok(())
    }
    pub fn status(&mut self, status: Status) -> Result<(), ToolError> {
        self.flush();
        let goal = self
            .state
            .goal
            .as_mut()
            .ok_or_else(|| ToolError("No goal is set.".into()))?;
        goal.status = status;
        if let Some(run) = &mut self.run
            && run.goal_id.as_ref() == Some(&goal.id)
        {
            run.clock = (status == Status::Active).then(Instant::now);
        }
        self.state.deferred = false;
        self.wanted = false;
        self.changed();
        Ok(())
    }
    pub fn begin(&mut self, automatic: bool, retry: bool) {
        if !automatic {
            self.state.deferred = false;
        }
        let bound = self.state.goal.as_ref().filter(|g| {
            g.status == Status::Active
                || retry && matches!(g.status, Status::Blocked | Status::UsageLimited)
        });
        self.run = Some(Run {
            goal_id: bound.map(|g| g.id.clone()),
            automatic,
            tools: false,
            clock: bound.map(|_| Instant::now()),
            messages: HashSet::new(),
            error: None,
        });
        self.wanted = false;
        self.revision = self.revision.wrapping_add(1);
    }
    pub fn assistant(&mut self, id: &str, usage: Option<Usage>, tools: bool) {
        let Some(run) = &mut self.run else {
            return;
        };
        run.tools |= tools;
        let Some(goal) = &mut self.state.goal else {
            return;
        };
        if run.goal_id.as_ref() != Some(&goal.id) || !run.messages.insert(id.into()) {
            return;
        }
        if let Some(usage) = usage.filter(|u| !u.partial) {
            goal.tokens_used = goal
                .tokens_used
                .saturating_add(usage.input)
                .saturating_add(usage.cache_write)
                .saturating_add(usage.output);
        } else {
            goal.usage_complete = false;
        }
        self.changed();
    }
    pub fn finish(
        &mut self,
        status: tiechui_plugin_api::protocol::TurnStatus,
    ) -> Result<(), ToolError> {
        self.flush();
        let bound = self.run.as_ref().is_some_and(|run| {
            self.state
                .goal
                .as_ref()
                .is_some_and(|g| run.goal_id.as_ref() == Some(&g.id))
        });
        if bound && self.active() {
            use tiechui_plugin_api::protocol::TurnStatus;
            match status {
                TurnStatus::Interrupted => self.status(Status::Paused)?,
                TurnStatus::Failed => {
                    let error = self
                        .run
                        .as_ref()
                        .and_then(|r| r.error.as_deref())
                        .unwrap_or("")
                        .to_lowercase();
                    let quota = [
                        "insufficient_quota",
                        "quota exceeded",
                        "quota exhausted",
                        "usage limit",
                        "credit exhausted",
                        "credits exhausted",
                        "credit depleted",
                        "credits depleted",
                    ]
                    .iter()
                    .any(|needle| error.contains(needle));
                    self.status(if quota {
                        Status::UsageLimited
                    } else {
                        Status::Blocked
                    })?;
                }
                _ => {
                    if self.run.as_ref().is_some_and(|r| r.automatic && !r.tools) {
                        self.state.deferred = true;
                    }
                }
            }
        }
        self.run = None;
        self.wanted = self.active() && !self.state.deferred;
        Ok(())
    }
}

// Gregorian civil date from UTC days, keeping persisted timestamps independent of locale.
fn timestamp() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let days = (seconds / 86400) as i64 + 719468;
    let era = days / 146097;
    let doe = days - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    let year = year + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.000Z",
        seconds / 3600 % 24,
        seconds / 60 % 60,
        seconds % 60
    )
}
fn valid_date(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() != 24
        || b[4] != b'-'
        || b[7] != b'-'
        || b[10] != b'T'
        || b[13] != b':'
        || b[16] != b':'
        || b[19] != b'.'
        || b[23] != b'Z'
    {
        return false;
    }
    let number = |a: usize, z: usize| s.get(a..z).and_then(|s| s.parse::<u32>().ok());
    let (Some(year), Some(month), Some(day), Some(hour), Some(minute), Some(second), Some(_)) = (
        number(0, 4),
        number(5, 7),
        number(8, 10),
        number(11, 13),
        number(14, 16),
        number(17, 19),
        number(20, 23),
    ) else {
        return false;
    };
    let days = match month {
        2 => {
            if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) {
                29
            } else {
                28
            }
        }
        4 | 6 | 9 | 11 => 30,
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        _ => 0,
    };
    day > 0 && day <= days && hour < 24 && minute < 60 && second < 60
}
