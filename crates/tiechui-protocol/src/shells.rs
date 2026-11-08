use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const SHELLS_EVENT: &str = "tiechui.exec.tasks";

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ShellState {
    Starting,
    Running,
    Stopping,
    Completed,
    Failed,
    Cancelled,
    TimedOut,
}

impl ShellState {
    pub fn active(self) -> bool {
        matches!(self, Self::Starting | Self::Running | Self::Stopping)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Stopping => "stopping",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::TimedOut => "timed out",
        }
    }
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ShellMode {
    Foreground,
    Background,
}

impl ShellMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Foreground => "foreground",
            Self::Background => "background",
        }
    }
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ShellTask {
    pub id: String,
    pub command: String,
    pub cwd: String,
    pub mode: ShellMode,
    pub state: ShellState,
    /// Started by the user, e.g. `!command`, rather than the model.
    pub user: bool,
    /// Milliseconds since the Unix epoch.
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub exit_code: Option<i32>,
}

impl ShellTask {
    /// Run time up to `now` (milliseconds since the Unix epoch), or until it ended: `12s`, `01:42`.
    pub fn elapsed(&self, now: i64) -> String {
        let seconds = (self.ended_at.unwrap_or(now) - self.started_at).max(0) / 1000;
        if seconds < 60 {
            format!("{seconds}s")
        } else if seconds < 3600 {
            format!("{:02}:{:02}", seconds / 60, seconds % 60)
        } else {
            format!(
                "{}:{:02}:{:02}",
                seconds / 3600,
                seconds / 60 % 60,
                seconds % 60
            )
        }
    }
}

/// The retained output tail of one task, sent when a client asks for it.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ShellDetail {
    pub id: String,
    pub output: String,
    pub log_path: Option<String>,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ShellsView {
    pub revision: u64,
    pub tasks: Vec<ShellTask>,
    pub open: bool,
    pub detail: Option<ShellDetail>,
}

impl ShellsView {
    /// Background tasks still holding a process; foreground commands keep their tool cards.
    pub fn background(&self) -> usize {
        self.tasks
            .iter()
            .filter(|task| task.mode == ShellMode::Background && task.state.active())
            .count()
    }
}
