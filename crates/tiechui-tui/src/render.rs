//! Inline rendering: history goes to native scrollback, the live area is redrawn below it.

use std::io::{self, Write};

use ratatui_core::style::{Color, Modifier, Style};
use ratatui_core::text::Line;

fn color(code: &mut Vec<String>, color: Color, base: u8) {
    let (normal, bright, extended) = (base, base + 60, base + 8);
    let value = match color {
        Color::Reset => return,
        Color::Black => normal.to_string(),
        Color::Red => (normal + 1).to_string(),
        Color::Green => (normal + 2).to_string(),
        Color::Yellow => (normal + 3).to_string(),
        Color::Blue => (normal + 4).to_string(),
        Color::Magenta => (normal + 5).to_string(),
        Color::Cyan => (normal + 6).to_string(),
        Color::Gray => (normal + 7).to_string(),
        Color::DarkGray => bright.to_string(),
        Color::LightRed => (bright + 1).to_string(),
        Color::LightGreen => (bright + 2).to_string(),
        Color::LightYellow => (bright + 3).to_string(),
        Color::LightBlue => (bright + 4).to_string(),
        Color::LightMagenta => (bright + 5).to_string(),
        Color::LightCyan => (bright + 6).to_string(),
        Color::White => (bright + 7).to_string(),
        Color::Indexed(index) => format!("{extended};5;{index}"),
        Color::Rgb(r, g, b) => format!("{extended};2;{r};{g};{b}"),
    };
    code.push(value);
}

pub fn sgr(style: Style) -> String {
    let mut code = vec!["0".to_owned()];
    for (modifier, value) in [
        (Modifier::BOLD, "1"),
        (Modifier::DIM, "2"),
        (Modifier::ITALIC, "3"),
        (Modifier::UNDERLINED, "4"),
        (Modifier::REVERSED, "7"),
        (Modifier::CROSSED_OUT, "9"),
    ] {
        if style.add_modifier.contains(modifier) {
            code.push(value.to_owned());
        }
    }
    if let Some(fg) = style.fg {
        color(&mut code, fg, 30);
    }
    if let Some(bg) = style.bg {
        color(&mut code, bg, 40);
    }
    format!("\x1b[{}m", code.join(";"))
}

/// One line with explicit resets, so no style leaks into the next line or the shell.
pub fn ansi(line: &Line<'_>) -> String {
    let mut out = String::new();
    for span in &line.spans {
        let style = line.style.patch(span.style);
        if style == Style::default() {
            out.push_str("\x1b[0m");
        } else {
            out.push_str(&sgr(style));
        }
        out.push_str(&span.content);
    }
    out.push_str("\x1b[0m");
    out
}

/// Tracks where the live area starts so it can be cleared and redrawn in place.
#[derive(Debug, Default)]
pub struct Screen {
    /// Rows between the top of the live area and the cursor.
    cursor_row: usize,
}

impl Screen {
    fn rewind(&self, out: &mut impl Write) -> io::Result<()> {
        if self.cursor_row > 0 {
            write!(out, "\x1b[{}A", self.cursor_row)?;
        }
        write!(out, "\r\x1b[J")
    }

    /// Prints `history` above the live area and redraws `live`, keeping its tail when taller than
    /// the terminal. `cursor` is a (row, column) inside `live`.
    pub fn frame(
        &mut self,
        out: &mut impl Write,
        history: &[Line<'static>],
        live: &[Line<'static>],
        cursor: Option<(usize, usize)>,
        height: usize,
    ) -> io::Result<()> {
        write!(out, "\x1b[?2026h")?;
        self.rewind(out)?;
        for line in history {
            write!(out, "{}\r\n", ansi(line))?;
        }
        let skip = live.len().saturating_sub(height.max(1));
        let visible = &live[skip..];
        for (index, line) in visible.iter().enumerate() {
            if index > 0 {
                write!(out, "\r\n")?;
            }
            write!(out, "{}", ansi(line))?;
        }
        let last = visible.len().saturating_sub(1);
        let (row, column) = cursor
            .map(|(row, column)| (row.saturating_sub(skip).min(last), column))
            .unwrap_or((last, 0));
        if last > row {
            write!(out, "\x1b[{}A", last - row)?;
        }
        write!(out, "\r")?;
        if column > 0 {
            write!(out, "\x1b[{column}C")?;
        }
        write!(
            out,
            "{}",
            if cursor.is_some() {
                "\x1b[?25h"
            } else {
                "\x1b[?25l"
            }
        )?;
        self.cursor_row = row;
        write!(out, "\x1b[?2026l")?;
        out.flush()
    }

    /// Clears the screen and its scrollback for another session's transcript in one
    /// sequence: Terminal.app and Warp may keep scrollback otherwise.
    pub fn wipe(&mut self, out: &mut impl Write) -> io::Result<()> {
        self.cursor_row = 0;
        write!(out, "\x1b[r\x1b[0m\x1b[H\x1b[2J\x1b[3J\x1b[H")?;
        out.flush()
    }

    /// Removes the live area, leaving the cursor where the next shell prompt belongs.
    pub fn clear(&mut self, out: &mut impl Write) -> io::Result<()> {
        self.rewind(out)?;
        self.cursor_row = 0;
        write!(out, "\x1b[?25h")?;
        out.flush()
    }
}

/// Draws a full-screen page (the transcript pager on the alternate screen).
pub fn page(out: &mut impl Write, lines: &[Line<'static>]) -> io::Result<()> {
    Canvas::default().draw(out, lines, None)
}

/// The alternate screen as last drawn, so a frame writes only the rows that changed.
#[derive(Debug, Default)]
pub struct Canvas {
    /// `None` until the next draw repaints everything.
    rows: Option<Vec<String>>,
    cursor: Option<(usize, usize)>,
}

impl Canvas {
    /// Call after anything else changed the screen: a resize, or leaving the alternate screen.
    pub fn invalidate(&mut self) {
        self.rows = None;
    }

    pub fn draw(
        &mut self,
        out: &mut impl Write,
        lines: &[Line<'static>],
        cursor: Option<(usize, usize)>,
    ) -> io::Result<()> {
        let rows: Vec<String> = lines.iter().map(ansi).collect();
        let previous = self.rows.take();
        if previous.as_ref() == Some(&rows) && self.cursor == cursor {
            self.rows = previous;
            return Ok(());
        }
        write!(out, "\x1b[?2026h\x1b[?25l")?;
        let previous = match previous {
            Some(previous) => previous,
            None => {
                write!(out, "\x1b[H\x1b[J")?;
                Vec::new()
            }
        };
        for (index, row) in rows.iter().enumerate() {
            if previous.get(index) != Some(row) {
                write!(out, "\x1b[{};1H{row}\x1b[K", index + 1)?;
            }
        }
        if rows.len() < previous.len() {
            write!(out, "\x1b[{};1H\x1b[J", rows.len() + 1)?;
        }
        if let Some((row, column)) = cursor {
            write!(out, "\x1b[{};{}H\x1b[?25h", row + 1, column + 1)?;
        }
        write!(out, "\x1b[?2026l")?;
        out.flush()?;
        self.rows = Some(rows);
        self.cursor = cursor;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draw(canvas: &mut Canvas, rows: &[&str], cursor: Option<(usize, usize)>) -> String {
        let lines: Vec<Line<'static>> = rows.iter().map(|row| Line::raw(row.to_string())).collect();
        let mut out = Vec::new();
        canvas.draw(&mut out, &lines, cursor).unwrap_or_default();
        String::from_utf8(out).unwrap_or_default()
    }

    #[test]
    fn canvas_writes_only_changed_rows_until_invalidated() {
        let mut canvas = Canvas::default();
        let first = draw(&mut canvas, &["a", "b", "c"], None);
        assert!(first.contains("\x1b[H\x1b[J") && first.contains("\x1b[3;1H"));
        assert_eq!(draw(&mut canvas, &["a", "b", "c"], None), "");
        let changed = draw(&mut canvas, &["a", "B", "c"], None);
        assert!(changed.contains("\x1b[2;1H") && !changed.contains("\x1b[1;1H"));
        assert!(!changed.contains("\x1b[H\x1b[J"));
        let shorter = draw(&mut canvas, &["a"], Some((0, 1)));
        assert!(
            shorter.contains("\x1b[2;1H\x1b[J")
                && shorter.ends_with("\x1b[1;2H\x1b[?25h\x1b[?2026l")
        );
        assert!(draw(&mut canvas, &["a"], Some((0, 2))).contains("\x1b[1;3H"));
        canvas.invalidate();
        assert!(draw(&mut canvas, &["a"], Some((0, 2))).contains("\x1b[H\x1b[J"));
    }
}
