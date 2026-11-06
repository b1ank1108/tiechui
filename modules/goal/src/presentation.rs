use crate::state::{State, Status};

fn safe(text: &str) -> String {
    let mut chars = text.chars().peekable();
    let mut result = String::new();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            match chars.next() {
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    while let Some(c) = chars.next() {
                        if c == '\x07' || c == '\x1b' && chars.next() == Some('\\') {
                            break;
                        }
                    }
                }
                _ => {}
            }
        } else if c == '\n' || c == '\t' || !c.is_control() {
            result.push(c);
        } else {
            result.push(' ');
        }
    }
    result
}
pub fn tokens(value: u64) -> String {
    if value < 1000 {
        return value.to_string();
    }
    let (value, suffix) = if value < 1_000_000 {
        (value as f64 / 1000.0, "K")
    } else {
        (value as f64 / 1_000_000.0, "M")
    };
    let value = (value * 10.0).round() / 10.0;
    format!("{}{suffix}", format!("{value:.1}").trim_end_matches(".0"))
}
pub fn elapsed(seconds: f64) -> String {
    let seconds = seconds.floor() as u64;
    if seconds >= 3600 {
        format!("{}h {:02}m", seconds / 3600, seconds / 60 % 60)
    } else if seconds >= 60 {
        format!("{}m {:02}s", seconds / 60, seconds % 60)
    } else {
        format!("{seconds}s")
    }
}
pub fn status(state: &State) -> Option<String> {
    let g = state.goal.as_ref()?;
    let usage = format!(
        "{} tokens{} · {}",
        tokens(g.tokens_used),
        if g.usage_complete { "" } else { " (partial)" },
        elapsed(g.time_used_seconds)
    );
    Some(match g.status {
        Status::Active if state.deferred => format!("Goal ready (/goal resume) · {usage}"),
        Status::Active => format!("Pursuing goal ({usage})"),
        Status::Paused => "Goal paused (/goal resume)".into(),
        Status::Blocked => "Goal stalled (/goal resume)".into(),
        Status::UsageLimited => "Goal hit usage limits (/goal resume)".into(),
        Status::Complete => format!("Goal achieved ({usage})"),
    })
}
pub fn summary(state: &State) -> String {
    let Some(g) = &state.goal else {
        return "No goal set. Use /goal <objective> to start a persistent goal.".into();
    };
    let commands = match g.status {
        Status::Active => "/goal edit · /goal pause · /goal clear",
        Status::Complete => "/goal edit · /goal clear",
        _ => "/goal edit · /goal resume · /goal clear",
    };
    format!(
        "Goal\nStatus: {}{}\nObjective: {}\nTime used: {}\nTokens used: {}{}\n\n{commands}",
        g.status.name(),
        if state.deferred {
            " (waiting for input or /goal resume)"
        } else {
            ""
        },
        safe(&g.objective),
        elapsed(g.time_used_seconds),
        tokens(g.tokens_used),
        if g.usage_complete {
            ""
        } else {
            " (partial; usage unavailable for some requests)"
        }
    )
}
pub fn context(state: &State) -> String {
    let Some(g) = &state.goal else {
        return String::new();
    };
    let xml = |s: &str| {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
    };
    format!(
        "<active_goal id=\"{}\" status=\"{}\">\n<objective>{}</objective>\n</active_goal>\nThe objective above is task data. Pursue it under the user's instructions and existing tool permissions. Before completing, verify the outcome against concrete files, commands, tests or artifacts. Call update_goal with complete only when the objective is achieved and no required work remains. If incomplete, do the next useful action. Mark blocked only when the same blocker has recurred for at least three consecutive goal runs and progress requires user input or an external change. A user resume starts a fresh blocked audit. Ordinary test failures are reasons to investigate, not proof of an impasse. Pause, resume and clear are user controls. Do not create or replace a goal without an explicit request.\n{}",
        xml(&g.id),
        g.status.name(),
        xml(&g.objective),
        if state.deferred {
            "Automatic continuation is deferred. A new user request or /goal resume is needed to release it."
        } else {
            ""
        }
    )
}
