//! The working indicator: a neutral sweep on truecolor terminals, dot and weight fallbacks.

use std::time::Duration;

use ratatui_core::style::{Color, Modifier, Style};
use ratatui_core::text::Span;
use unicode_segmentation::UnicodeSegmentation;

use crate::palette::{ColorLevel, blend};
use crate::theme::Theme;

const SWEEP: f32 = 2000.0;

/// Seconds, minutes and seconds, or hours, minutes and seconds; seconds after minutes are padded.
pub fn elapsed(duration: Duration) -> String {
    let total = duration.as_secs();
    let (hours, minutes, seconds) = (total / 3600, (total / 60) % 60, total % 60);
    if hours > 0 {
        format!("{hours}h {minutes:02}m {seconds:02}s")
    } else if minutes > 0 {
        format!("{minutes}m {seconds:02}s")
    } else {
        format!("{seconds}s")
    }
}

/// A sweep passes over the graphemes every two seconds.
pub fn shimmer(text: &str, theme: &Theme, phase: Duration) -> Vec<Span<'static>> {
    let graphemes: Vec<&str> = text.graphemes(true).collect();
    let millis = (phase.as_millis() % 2000) as f32;
    let position = ((millis / SWEEP) * (graphemes.len() + 20) as f32).floor();
    let colors = (theme.palette.level == ColorLevel::TrueColor)
        .then_some(())
        .and(theme.palette.foreground.zip(theme.palette.background));
    graphemes
        .iter()
        .enumerate()
        .map(|(index, grapheme)| {
            let distance = (index as f32 + 10.0 - position).abs();
            let intensity = if distance <= 5.0 {
                (1.0 + (std::f32::consts::PI * distance / 5.0).cos()) / 2.0
            } else {
                0.0
            };
            let style = match colors {
                Some((foreground, background)) => {
                    let rgb = blend(background, foreground, intensity * 0.9);
                    Style::default()
                        .fg(Color::Rgb(rgb.0, rgb.1, rgb.2))
                        .add_modifier(Modifier::BOLD)
                }
                None if intensity < 0.2 => Style::default().add_modifier(Modifier::DIM),
                None if intensity < 0.6 => Style::default(),
                None => Style::default().add_modifier(Modifier::BOLD),
            };
            Span::styled((*grapheme).to_owned(), style)
        })
        .collect()
}

/// The bullet of running work: swept on truecolor, otherwise alternating `•` and dim `◦`.
pub fn bullet(theme: &Theme, phase: Duration, elapsed: Duration) -> Span<'static> {
    pulse("•", "◦", theme, phase, elapsed)
}

/// The bullet's motion on `◇`, the marker of context maintenance.
pub fn diamond(theme: &Theme, phase: Duration, elapsed: Duration) -> Span<'static> {
    pulse("◇", "◇", theme, phase, elapsed)
}

fn pulse(
    on: &'static str,
    off: &'static str,
    theme: &Theme,
    phase: Duration,
    elapsed: Duration,
) -> Span<'static> {
    if theme.palette.level == ColorLevel::TrueColor && theme.palette.background.is_some() {
        return shimmer(on, theme, phase).remove(0);
    }
    if (elapsed.as_millis() / 600) % 2 == 1 {
        Span::styled(off, Style::default().add_modifier(Modifier::DIM))
    } else {
        Span::raw(on)
    }
}

/// The first complete `**heading**` of a thinking block; an unclosed one is not a status.
pub fn heading(thinking: &str) -> Option<&str> {
    let start = thinking.find("**")? + 2;
    let end = start + thinking[start..].find("**")?;
    let title = thinking[start..end].trim();
    (!title.is_empty() && !title.contains('\n')).then_some(title)
}

/// Reasoning summary: a leading heading followed by a line break is a title and is
/// dropped from the summary. Untitled thinking is hidden unless it is only a bold conclusion.
pub fn reasoning(text: &str) -> (String, bool) {
    let text = text.trim();
    let mut content = text;
    let mut placeholder_title = false;
    if let Some(rest) = text.strip_prefix("**")
        && let Some(end) = rest.find("**")
    {
        let after = rest[end + 2..].trim();
        if after == "<!-- -->" {
            placeholder_title = true;
            content = "";
        }
    }
    if content.trim() == "<!-- -->" {
        content = "";
    }
    if let Some(rest) = content.strip_prefix("**")
        && let Some(end) = rest.find("**")
        && rest[end + 2..].starts_with(['\r', '\n'])
    {
        return (rest[end + 2..].trim().to_owned(), true);
    }
    let title_only = content.starts_with("**")
        && content.ends_with("**")
        && content.len() > 4
        && !content[2..content.len() - 2].contains("**");
    (content.to_owned(), placeholder_title || title_only)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elapsed_pads_seconds_after_minutes() {
        assert_eq!(elapsed(Duration::from_millis(999)), "0s");
        assert_eq!(elapsed(Duration::from_secs(59)), "59s");
        assert_eq!(elapsed(Duration::from_secs(60)), "1m 00s");
        assert_eq!(elapsed(Duration::from_secs(3599)), "59m 59s");
        assert_eq!(elapsed(Duration::from_secs(3600)), "1h 00m 00s");
    }

    #[test]
    fn headings_need_both_markers() {
        assert_eq!(
            heading("**Planning the fix**\n\nDetails"),
            Some("Planning the fix")
        );
        assert_eq!(
            heading("thinking about **Inspecting** files"),
            Some("Inspecting")
        );
        assert_eq!(heading("**Unclosed heading"), None);
        assert_eq!(heading("plain"), None);
    }

    #[test]
    fn reasoning_drops_the_title_and_hides_untitled_text() {
        assert_eq!(
            reasoning("**Checking tests**\n\nThe suite covers it."),
            ("The suite covers it.".into(), true)
        );
        assert_eq!(reasoning("just musing"), ("just musing".into(), false));
        assert_eq!(reasoning("**Done**"), ("**Done**".into(), true));
        assert_eq!(reasoning("**Title** <!-- -->"), (String::new(), true));
        assert_eq!(reasoning("<!-- -->"), (String::new(), false));
    }

    #[test]
    fn shimmer_falls_back_to_weights_without_truecolor() {
        let theme = Theme::fixture(ColorLevel::Ansi256, false);
        let spans = shimmer("Working", &theme, Duration::from_millis(1000));
        assert_eq!(spans.len(), 7);
        assert!(spans.iter().all(|s| s.style.fg.is_none()));
        let truecolor = Theme::fixture(ColorLevel::TrueColor, false);
        assert!(
            shimmer("Working", &truecolor, Duration::ZERO)
                .iter()
                .all(|s| matches!(s.style.fg, Some(Color::Rgb(..))))
        );
        assert_eq!(
            bullet(&theme, Duration::ZERO, Duration::from_millis(700)).content,
            "◦"
        );
    }
}
