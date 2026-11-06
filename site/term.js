// Renders the snapshot markup of crates/tiechui-tui/tests/support::dump as terminal cells.
// `[b,d,i,u,s,fg<color>,bg<color>]text` styles one span; colors are `#rrggbb`, `@index` or
// ratatui names. The gallery adds `shimmer` for live states.
(() => {
  const COLOR = "(?:#[0-9a-f]{6}|@\\d{1,3}|[A-Z][A-Za-z]+)";
  const ITEM = `(?:[bdius]|fg${COLOR}|bg${COLOR}|shimmer)`;
  const TAG = new RegExp(`\\[(-|${ITEM}(?:,${ITEM})*)\\]`, "g");
  const CURSOR = /^\[cursor (?:None|Some\(\((\d+), (\d+)\)\))\]$/;

  const FIXTURE = {
    dark: { fg: "#d0d0d0", bg: "#1e1e1e" },
    light: { fg: "#1f2328", bg: "#fafafa" },
  };
  // The terminal's own 16 slots are unknown; these stay close to Mist so fallbacks read true.
  const ANSI = {
    dark: ["#3b3f45", "#d47a7a", "#8fbf8f", "#d6b07a", "#7fa4d6", "#b39ddb", "#6fc0c8", "#c8ccd2",
      "#666c77", "#e39a9a", "#a8d4a8", "#e6c48f", "#9ab9e3", "#c7b3e6", "#8fd3da", "#eceff3"],
    light: ["#2b2f36", "#b84a4a", "#3f8a4f", "#9a7224", "#3d68a8", "#7d5aa8", "#2a8b95", "#5f6672",
      "#9aa1ac", "#c95f5f", "#4f9a5f", "#b08a3a", "#4f7ac0", "#9070bf", "#3a9ea8", "#2b2f36"],
  };
  const NAMES = ["Black", "Red", "Green", "Yellow", "Blue", "Magenta", "Cyan", "Gray", "DarkGray",
    "LightRed", "LightGreen", "LightYellow", "LightBlue", "LightMagenta", "LightCyan", "White"];

  const hex = (value) => [1, 3, 5].map((i) => parseInt(value.slice(i, i + 2), 16));
  const css = ([r, g, b]) => `#${[r, g, b].map((v) => v.toString(16).padStart(2, "0")).join("")}`;
  // palette.rs `blend`: channel truncation included.
  const blend = (top, base, alpha) =>
    css(hex(top).map((t, i) => Math.floor(t * alpha + hex(base)[i] * (1 - alpha))));

  function indexed(n, mode) {
    if (n < 16) return ANSI[mode][n];
    if (n >= 232) {
      const v = 8 + (n - 232) * 10;
      return css([v, v, v]);
    }
    const cube = [0, 95, 135, 175, 215, 255];
    const i = n - 16;
    return css([cube[Math.floor(i / 36)], cube[Math.floor(i / 6) % 6], cube[i % 6]]);
  }

  function color(value, mode) {
    if (value.startsWith("#")) return value;
    if (value.startsWith("@")) return indexed(Number(value.slice(1)), mode);
    const slot = NAMES.indexOf(value);
    return slot < 0 ? null : ANSI[mode][slot];
  }

  function style(tag) {
    const s = {};
    if (tag === "-") return s;
    for (const item of tag.split(",")) {
      if (item.startsWith("fg")) s.fg = item.slice(2);
      else if (item.startsWith("bg")) s.bg = item.slice(2);
      else s[item] = true;
    }
    return s;
  }

  const segmenter = new Intl.Segmenter(undefined, { granularity: "grapheme" });
  const WIDE = [[0x1100, 0x115f], [0x2e80, 0x303e], [0x3041, 0x33ff], [0x3400, 0x4dbf],
    [0x4e00, 0x9fff], [0xa000, 0xa4cf], [0xac00, 0xd7a3], [0xf900, 0xfaff], [0xfe30, 0xfe4f],
    [0xff00, 0xff60], [0xffe0, 0xffe6], [0x20000, 0x3fffd]];
  const EMOJI = /\p{Emoji_Presentation}/u;
  const wide = (g) => {
    const code = g.codePointAt(0);
    return EMOJI.test(g) || WIDE.some(([low, high]) => code >= low && code <= high);
  };
  // ASCII, box drawing and block elements sit on the grid in every frame font; others get a fixed cell.
  const native = (g) => {
    const code = g.codePointAt(0);
    return g.length === 1 && ((code >= 0x20 && code <= 0x7e) || (code >= 0x2500 && code <= 0x259f));
  };

  function cells(line) {
    const spans = [];
    let last = 0;
    let current = {};
    for (const match of line.matchAll(TAG)) {
      if (match.index > last) spans.push([current, line.slice(last, match.index)]);
      current = style(match[1]);
      last = match.index + match[0].length;
    }
    if (last < line.length) spans.push([current, line.slice(last)]);
    const out = [];
    for (const [s, text] of spans) {
      for (const { segment } of segmenter.segment(text)) {
        out.push({ g: segment, w: wide(segment) ? 2 : 1, s });
      }
    }
    return out;
  }

  /** Parses a frame: lines of cells and the cursor, if the snapshot recorded one. */
  function parse(lines) {
    let cursor = null;
    const rows = [];
    for (const line of lines) {
      const mark = CURSOR.exec(line);
      if (mark) {
        if (mark[1] !== undefined) cursor = { row: Number(mark[1]), col: Number(mark[2]) };
        continue;
      }
      rows.push(cells(line));
    }
    return { rows, cursor };
  }

  const width = (row) => row.reduce((sum, cell) => sum + cell.w, 0);

  function paint(cell, mode, base) {
    const s = cell.s;
    let fg = (s.fg && color(s.fg, mode)) || base.fg;
    const bg = s.bg && color(s.bg, mode);
    if (s.d) fg = blend(fg, bg || base.bg, 0.5);
    const rules = [];
    if (s.fg || s.d) rules.push(`color:${fg}`);
    if (bg) rules.push(`background:${bg}`);
    if (s.b) rules.push("font-weight:700");
    if (s.i) rules.push("font-style:italic");
    const lines = [s.u && "underline", s.s && "line-through"].filter(Boolean);
    if (lines.length) rules.push(`text-decoration:${lines.join(" ")}`);
    return { css: rules.join(";"), fg, bg: bg || base.bg };
  }

  // Painted rather than drawn from the font, so stacked half blocks touch like terminal cells.
  const BLOCKS = { "▀": [true, false], "▄": [false, true], "█": [true, true] };
  function block(g, { fg, bg }) {
    const [top, bottom] = BLOCKS[g].map((on) => (on ? fg : bg));
    const box = document.createElement("span");
    box.className = "px";
    box.style.background = `linear-gradient(${top} 50%, ${bottom} 50%)`;
    return box;
  }

  function glyph(cell, parent) {
    if (native(cell.g)) {
      parent.append(cell.g);
      return;
    }
    const box = document.createElement("span");
    box.className = cell.w === 2 ? "c c2" : "c";
    box.textContent = cell.g;
    parent.append(box);
  }

  /** Draws `frame` into `target`; `mode` is "dark" or "light". */
  function render(target, frame, { mode = "dark", cols } = {}) {
    const base = FIXTURE[mode];
    const columns = cols || Math.max(1, ...frame.rows.map(width));
    target.replaceChildren();
    target.style.setProperty("--cols", columns);
    target.style.setProperty("--term-fg", base.fg);
    target.style.setProperty("--term-bg", base.bg);
    frame.rows.forEach((row, index) => {
      const line = document.createElement("div");
      line.className = "ln";
      let cellsInRow = row;
      if (frame.cursor && frame.cursor.row === index) {
        const pad = frame.cursor.col + 1 - width(row);
        cellsInRow = pad > 0 ? row.concat(Array.from({ length: pad }, () => ({ g: " ", w: 1, s: {} }))) : row;
      }
      let column = 0;
      let run = null;
      let runStyle = null;
      let runPaint = null;
      for (const cell of cellsInRow) {
        const atCursor = frame.cursor && frame.cursor.row === index && frame.cursor.col === column;
        column += cell.w;
        if (atCursor) {
          const block = document.createElement("span");
          block.className = "cur";
          glyph(cell, block);
          line.append(block);
          run = null;
          continue;
        }
        if (!run || runStyle !== cell.s) {
          run = document.createElement("span");
          runStyle = cell.s;
          const p = paint(cell, mode, base);
          runPaint = p;
          if (p.css) run.style.cssText = p.css;
          if (cell.s.shimmer) {
            run.classList.add("shimmer");
            run.style.setProperty("--sweep-fg", p.fg);
            run.style.setProperty("--sweep-bg", p.bg);
            run.style.setProperty("--n", cellsInRow.filter((c) => c.s === cell.s).length);
          }
          line.append(run);
        }
        // background-clip: text skips inline-block children, so swept text stays unboxed.
        if (cell.s.shimmer) run.append(cell.g);
        else if (BLOCKS[cell.g]) run.append(block(cell.g, runPaint));
        else glyph(cell, run);
      }
      target.append(line);
    });
  }

  /** Plain text of a frame, for copying. */
  function text(frame) {
    return frame.rows.map((row) => row.map((cell) => cell.g).join("").trimEnd()).join("\n");
  }

  window.TcTerm = { parse, render, text, blend, FIXTURE };
})();
