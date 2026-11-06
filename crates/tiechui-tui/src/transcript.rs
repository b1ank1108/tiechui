//! The conversation as cells: streaming answers, thinking, live tool groups, and the
//! scrollback lines they commit. The transcript pager renders the same cells expanded.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use ratatui_core::text::Line;
use serde_json::Value;
use tiechui_protocol::{InputContent, Item, ItemId, KnownItem, Turn, TurnStatus};

use crate::activity;
use crate::cells::Cell;
use crate::header::Header;
use crate::stream::Stream;
use crate::theme::Theme;
use crate::tools::{self, Tool, View};

pub struct Transcript {
    theme: Theme,
    width: usize,
    cells: Vec<Cell>,
    /// The full-screen rendering of the first `.0` cells, and each control row's line with its
    /// cell and the cell's first line.
    retained: (usize, Vec<Line<'static>>, Vec<Control>),
    /// Cells expanded in place by their control rows.
    unfolded: HashSet<usize>,
    history: Vec<Line<'static>>,
    printed: bool,
    stream: Option<(ItemId, Stream)>,
    thinking: Option<(ItemId, String)>,
    status: Option<String>,
    /// Answers already committed from their stream, so their completion is not repeated.
    streamed: Vec<ItemId>,
    tools: Vec<Tool>,
    results: HashMap<ItemId, String>,
    /// Tool activity since the last answer: the next answer gets a separator.
    after_tools: bool,
    turn_tools: bool,
    show_thinking: bool,
    reflow: Option<Instant>,
    stream_reflow: bool,
    /// Where the session header goes once its details arrive: cell and line indices, and
    /// whether anything was printed before it. Scrollback is held until then.
    header: Option<(usize, usize, bool)>,
}

impl Transcript {
    pub fn new(theme: Theme, width: usize) -> Self {
        Self {
            theme,
            width: width.max(1),
            cells: Vec::new(),
            retained: Default::default(),
            unfolded: HashSet::new(),
            history: Vec::new(),
            printed: false,
            stream: None,
            thinking: None,
            status: None,
            streamed: Vec::new(),
            tools: Vec::new(),
            results: HashMap::new(),
            after_tools: false,
            turn_tools: false,
            show_thinking: false,
            reflow: None,
            stream_reflow: false,
            header: None,
        }
    }

    pub fn take_history(&mut self) -> Vec<Line<'static>> {
        if self.header.is_some() {
            return Vec::new();
        }
        std::mem::take(&mut self.history)
    }

    pub fn discard_history(&mut self) {
        self.history.clear();
        self.reflow = None;
        self.stream_reflow = false;
        if let Some((_, line, _)) = &mut self.header {
            // The pending header's insertion point belonged to the discarded output batch.
            *line = 0;
        }
    }

    /// Reserves the header's place before what follows; `header` fills it.
    pub fn hold_header(&mut self) {
        self.header = Some((self.cells.len(), self.history.len(), self.printed));
        self.printed = true;
    }

    pub fn header_pending(&self) -> bool {
        self.header.is_some()
    }

    pub fn header(&mut self, header: Header) {
        let Some((cell, line, printed)) = self.header.take() else {
            return;
        };
        let header = Cell::Header(header);
        let mut lines = header.render(self.width, &self.theme, View::History, false);
        if printed {
            lines.insert(0, Line::default());
        }
        self.history.splice(line..line, lines);
        self.cells.insert(cell, header);
        self.unfolded = self
            .unfolded
            .iter()
            .map(|&i| i + usize::from(i >= cell))
            .collect();
        self.retained = Default::default();
    }

    pub fn resize(&mut self, width: usize) {
        if self.width != width.max(1) {
            self.retained = Default::default();
        }
        self.width = width.max(1);
        if let Some((_, stream)) = &mut self.stream {
            stream.resize(self.width, &self.theme);
        }
    }

    pub fn schedule_reflow(&mut self, now: Instant) {
        if !self.printed {
            return;
        }
        self.reflow = Some(now + Duration::from_millis(75));
        self.stream_reflow |= self.stream.is_some();
    }

    pub fn reflow_pending(&self) -> bool {
        self.reflow.is_some()
    }

    pub fn set_theme(&mut self, theme: Theme) {
        if self.theme != theme {
            self.retained = Default::default();
            self.theme = theme;
            self.resize(self.width);
        }
    }

    /// Rebuild only the retained scrollback suffix, including already committed stream rows.
    pub fn reflow(&mut self, now: Instant, cap: usize) -> Option<Vec<Line<'static>>> {
        if self.header.is_some() || self.reflow.is_none_or(|deadline| deadline > now) {
            return None;
        }
        self.reflow = None;
        self.history.clear();
        let mut blocks = std::collections::VecDeque::new();
        let mut count = 0;
        if let Some((_, stream)) = &self.stream {
            let lines = stream.history(self.width, &self.theme);
            count += lines.len();
            blocks.push_front(lines);
        }
        for cell in self.cells.iter().rev() {
            if count >= cap {
                break;
            }
            let mut block = cell.render(self.width, &self.theme, View::History, self.show_thinking);
            if !block.is_empty() {
                if count > 0 {
                    block.push(Line::default());
                }
                count += block.len();
                blocks.push_front(block);
            }
        }
        Some(
            blocks
                .into_iter()
                .flatten()
                .skip(count.saturating_sub(cap))
                .collect(),
        )
    }

    pub fn toggle_thinking(&mut self) -> bool {
        self.retained = Default::default();
        self.show_thinking = !self.show_thinking;
        self.show_thinking
    }

    /// True while an answer streams; the working status hides then.
    pub fn answering(&self) -> bool {
        self.stream.is_some()
    }

    /// The first complete heading of the current thinking block, while no tool or answer
    /// has started since.
    pub fn status(&self) -> Option<&str> {
        self.status.as_deref()
    }

    fn separate(&mut self) {
        if self.printed {
            self.history.push(Line::default());
        }
        self.printed = true;
    }

    fn push(&mut self, cell: Cell) {
        let lines = cell.render(self.width, &self.theme, View::History, self.show_thinking);
        if !lines.is_empty() {
            self.separate();
            self.history.extend(lines);
        }
        self.cells.push(cell);
    }

    /// Before an answer's first visible line: flush tools and draw the separator.
    fn begin_answer(&mut self) {
        self.flush(true);
        if std::mem::take(&mut self.after_tools) {
            self.push(Cell::Separator);
        }
        self.separate();
    }

    /// Commits finished groups in call order; a group can still grow while it is last.
    fn flush(&mut self, all: bool) {
        let groups = tools::groups(&self.tools);
        let ready = if all {
            groups.len()
        } else {
            groups
                .iter()
                .take(groups.len().saturating_sub(1))
                .take_while(|group| self.tools[(*group).clone()].iter().all(Tool::done))
                .count()
        };
        let Some(end) = ready.checked_sub(1).map(|last| groups[last].end) else {
            return;
        };
        let flushed: Vec<Tool> = self.tools.drain(..end).collect();
        for group in groups.into_iter().take(ready) {
            self.push(Cell::Tools(flushed[group].to_vec()));
        }
    }

    fn finish_stream(&mut self, text: Option<&str>) {
        let Some((id, stream)) = self.stream.take() else {
            return;
        };
        self.streamed.push(id);
        let text = text.map_or_else(|| stream.source().to_owned(), str::to_owned);
        let started = stream.committed();
        let lines = stream.finish(&text, self.width, &self.theme);
        if !started && !lines.is_empty() {
            self.begin_answer();
        }
        self.history.extend(lines);
        if !text.trim().is_empty() {
            self.cells.push(Cell::Assistant(text));
        }
        if std::mem::take(&mut self.stream_reflow) {
            self.reflow = Some(Instant::now());
        }
    }

    fn tool(&mut self, id: &str) -> Option<&mut Tool> {
        self.tools.iter_mut().find(|tool| tool.id == id)
    }

    fn add_tool(&mut self, id: String, name: String, arguments: Value) {
        self.finish_stream(None);
        self.status = None;
        self.after_tools = true;
        self.turn_tools = true;
        match self.tool(&id) {
            Some(tool) => {
                tool.name = name;
                if arguments.as_object().is_none_or(|args| !args.is_empty()) {
                    tool.arguments = arguments;
                }
            }
            None => self.tools.push(Tool::new(id, name, arguments)),
        }
    }

    pub fn started(&mut self, item: KnownItem, now: Instant) {
        match item {
            KnownItem::AssistantMessage { id, text } => {
                self.finish_stream(None);
                self.flush(true);
                self.status = None;
                let mut stream = Stream::default();
                stream.push(&text);
                self.stream = Some((id, stream));
            }
            KnownItem::Thinking { id, text } => {
                self.status = activity::heading(&text).map(str::to_owned);
                self.thinking = Some((id, text));
            }
            KnownItem::ToolCall {
                id,
                name,
                arguments,
            } => self.add_tool(id.0, name, arguments),
            KnownItem::ToolResult { id, call_id, .. } => {
                if let Some(tool) = self.tool(&call_id.0) {
                    tool.started = Some(now);
                }
                self.results.insert(id, call_id.0);
            }
            KnownItem::UserMessage { .. } | KnownItem::Compaction { .. } => {}
        }
    }

    pub fn delta(&mut self, item_id: &ItemId, delta: &str) {
        if let Some((id, stream)) = &mut self.stream
            && id == item_id
        {
            stream.push(delta);
            let started = stream.committed();
            let lines = stream.commit(self.width, &self.theme);
            if !started && !lines.is_empty() {
                self.begin_answer();
            }
            self.history.extend(lines);
        } else if let Some((id, text)) = &mut self.thinking
            && id == item_id
        {
            text.push_str(delta);
            if self.status.is_none() {
                self.status = activity::heading(text).map(str::to_owned);
            }
        } else if let Some(call) = self.results.get(item_id).cloned()
            && let Ok(output) = serde_json::from_str::<Value>(delta)
            && let Some(tool) = self.tool(&call)
        {
            tool.partial = Some(output);
        }
    }

    pub fn completed(&mut self, item: KnownItem, now: Instant) {
        match item {
            KnownItem::Compaction {
                summary,
                tokens_before,
                degraded,
                ..
            } => {
                self.finish_stream(None);
                self.flush(true);
                self.push(Cell::Compaction {
                    summary,
                    tokens_before,
                    degraded,
                });
            }
            KnownItem::UserMessage { content, .. } => {
                self.finish_stream(None);
                self.flush(true);
                self.after_tools = false;
                let text: Vec<String> = content
                    .into_iter()
                    .map(|part| match part {
                        InputContent::Text { text } => text,
                        InputContent::Image { .. } => "[image]".into(),
                    })
                    .collect();
                self.push(Cell::User(text.join("\n")));
            }
            KnownItem::AssistantMessage { id, text } => {
                if self
                    .stream
                    .as_ref()
                    .is_some_and(|(stream, _)| *stream == id)
                {
                    self.finish_stream(Some(&text));
                } else if !text.trim().is_empty() && !self.streamed.contains(&id) {
                    self.flush(true);
                    if std::mem::take(&mut self.after_tools) {
                        self.push(Cell::Separator);
                    }
                    self.push(Cell::Assistant(text));
                }
            }
            KnownItem::Thinking { id, text } => {
                if self
                    .thinking
                    .as_ref()
                    .is_some_and(|(current, _)| *current == id)
                {
                    self.thinking = None;
                }
                let cell = Cell::Thinking(text);
                // Visible thinking ends the current tool group; hidden thinking does not.
                if !cell
                    .render(self.width, &self.theme, View::History, self.show_thinking)
                    .is_empty()
                {
                    self.flush(true);
                }
                self.push(cell);
            }
            KnownItem::ToolCall {
                id,
                name,
                arguments,
            } => self.add_tool(id.0, name, arguments),
            KnownItem::ToolResult {
                id,
                call_id,
                output,
                is_error,
            } => {
                self.results.remove(&id);
                if self.tool(&call_id.0).is_none() {
                    self.add_tool(call_id.0.clone(), "tool".into(), Value::Null);
                }
                if let Some(tool) = self.tool(&call_id.0) {
                    tool.duration = tool.started.map(|started| now - started);
                    tool.output = Some(output);
                    tool.partial = None;
                    tool.error = is_error;
                }
                self.flush(false);
            }
        }
    }

    /// Ends a turn; `worked` is the active time of a normally completed turn.
    pub fn turn_completed(&mut self, status: TurnStatus, worked: Option<Duration>) {
        self.finish_stream(None);
        self.flush(true);
        self.thinking = None;
        self.status = None;
        self.results.clear();
        self.streamed.clear();
        if status == TurnStatus::Interrupted {
            self.push(Cell::Interrupted);
        } else if status == TurnStatus::Completed
            && self.turn_tools
            && let Some(worked) = worked.filter(|w| w.as_secs() > 60)
        {
            self.push(Cell::Worked(worked));
        }
        self.turn_tools = false;
    }

    /// Forgets the conversation, unprinted lines included, for another session's.
    pub fn reset(&mut self) {
        *self = Self {
            show_thinking: self.show_thinking,
            ..Self::new(self.theme.clone(), self.width)
        };
    }

    /// A command's report, committed at once.
    pub fn info(&mut self, title: String, text: String) {
        self.push(Cell::Info { title, text });
    }

    pub fn error(&mut self, message: String) {
        self.finish_stream(None);
        self.flush(true);
        self.push(Cell::Error(message));
    }

    pub fn warning(&mut self, message: String) {
        self.finish_stream(None);
        self.flush(true);
        self.push(Cell::Warning(message));
    }

    /// Resumed history: the same cells, without banners or timing.
    pub fn replay(&mut self, turns: &[Turn], now: Instant) {
        for turn in turns {
            for item in &turn.items {
                if let Item::Known(item) = item {
                    self.completed(item.clone(), now);
                }
            }
            self.finish_stream(None);
            self.flush(true);
            self.turn_tools = false;
        }
    }

    /// The streaming tail and running tool groups for the live area.
    pub fn live(&self, phase: Duration, now: Instant) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        if let Some((_, stream)) = &self.stream {
            let tail = stream.tail(self.width, &self.theme);
            if !tail.is_empty() {
                if !stream.committed() && self.printed {
                    lines.push(Line::default());
                }
                lines.extend(tail);
            }
        }
        let view = View::Live { phase, now };
        for group in tools::groups(&self.tools) {
            if self.printed || !lines.is_empty() {
                lines.push(Line::default());
            }
            lines.extend(tools::render(
                &self.tools[group],
                self.width,
                &self.theme,
                view,
            ));
        }
        lines
    }

    pub fn tools_running(&self) -> bool {
        !self.tools.is_empty()
    }

    pub fn cell_count(&self) -> usize {
        self.cells.len()
    }

    /// Cache finished cells; steady-state frames only clone the visible rows.
    pub fn retained(&mut self) -> &[Line<'static>] {
        let (count, lines, controls) = &mut self.retained;
        for (index, cell) in self.cells.iter().enumerate().skip(*count) {
            let unfolded = self.unfolded.contains(&index);
            let (block, control) =
                cell.folded(self.width, &self.theme, unfolded, self.show_thinking);
            if !block.is_empty() {
                if !lines.is_empty() {
                    lines.push(Line::default());
                }
                if let Some(row) = control {
                    controls.push(Control {
                        line: lines.len() + row,
                        cell: index,
                        start: lines.len(),
                    });
                }
                lines.extend(block);
            }
        }
        *count = self.cells.len();
        lines
    }

    /// The control row at `line` of `retained`.
    pub fn control(&self, line: usize) -> Option<Control> {
        self.retained.2.iter().find(|c| c.line == line).copied()
    }

    /// Expands a folded cell in place, or folds it again.
    pub fn unfold(&mut self, cell: usize) {
        if !self.unfolded.remove(&cell) {
            self.unfolded.insert(cell);
        }
        self.retained = Default::default();
    }

    pub fn retained_live(&self, phase: Duration, now: Instant) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        if let Some((_, stream)) = &self.stream {
            push_block(
                &mut lines,
                crate::cells::assistant(stream.source(), self.width, &self.theme),
            );
        }
        for group in tools::groups(&self.tools) {
            push_block(
                &mut lines,
                tools::render(
                    &self.tools[group],
                    self.width,
                    &self.theme,
                    View::Live { phase, now },
                ),
            );
        }
        lines
    }

    /// Committed cells from `from`, expanded for the transcript pager, each after a blank row.
    pub fn expanded(&self, from: usize, width: usize) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        for cell in &self.cells[from.min(self.cells.len())..] {
            push_block(
                &mut lines,
                cell.render(width, &self.theme, View::Transcript, true),
            );
        }
        lines
    }

    /// Work still in progress, expanded like `expanded`.
    pub fn expanded_live(&self, width: usize) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        if let Some((_, text)) = &self.thinking {
            let cell = Cell::Thinking(text.clone());
            push_block(
                &mut lines,
                cell.render(width, &self.theme, View::Transcript, true),
            );
        }
        for group in tools::groups(&self.tools) {
            let rendered = tools::render(&self.tools[group], width, &self.theme, View::Transcript);
            push_block(&mut lines, rendered);
        }
        if let Some((_, stream)) = &self.stream {
            let cell = Cell::Assistant(stream.source().to_owned());
            push_block(&mut lines, cell.lines(width, &self.theme));
        }
        lines
    }
}

/// A control row in the full-screen transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Control {
    pub line: usize,
    pub cell: usize,
    /// The first line of the cell it expands.
    pub start: usize,
}

fn push_block(lines: &mut Vec<Line<'static>>, block: Vec<Line<'static>>) {
    if !block.is_empty() {
        lines.push(Line::default());
        lines.extend(block);
    }
}
