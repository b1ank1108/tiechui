// Gallery plates: which snapshot parts each exhibit shows and what to look for.
(() => {
  const DATA = window.TC_GALLERY;
  const Term = window.TcTerm;

  const MIST = {
    dark: { text: "#d6d9df", secondary: "#9aa0aa", tertiary: "#666c77", separator: "#33363d", accent: "#8fa8c9", accent2: "#a99ccf", success: "#8fb996", warning: "#d6b07a", error: "#d48a8a" },
    light: { text: "#2b2f36", secondary: "#5f6672", tertiary: "#9aa1ac", separator: "#dde0e5", accent: "#4f6f9a", accent2: "#7a68a8", success: "#4f8a5b", warning: "#a87a2f", error: "#b05656" },
  };
  const LEVEL = { TrueColor: "TrueColor", Ansi256: "256 色", Ansi16: "16 色" };
  const prefs = { 主题: "深", 列宽: "80" };
  const exhibits = [];

  // ---------- snapshot parts ----------

  function parts(names, match) {
    const out = [];
    for (const name of [].concat(names)) {
      const snap = DATA.snapshots[name];
      if (!snap) continue;
      for (const part of snap.parts) {
        if (match === undefined || (match instanceof RegExp ? match.test(part.title) : part.title === match)) {
          out.push({ name, snap, part });
        }
      }
    }
    return out;
  }

  /** Candidates from snapshot parts; `extra` adds axes such as a tab or a state. */
  function fromSnapshots(names, match, { extra, color, crop } = {}) {
    return parts(names, match).map(({ name, snap, part }) => {
      const mode = part.light ? "light" : "dark";
      const lines = color ? color(part.lines, MIST[mode], part) : part.lines;
      const axes = { 主题: part.light ? "浅" : "深", 色彩: LEVEL[snap.level] };
      if (part.cols) axes.列宽 = String(part.cols);
      Object.assign(axes, extra ? extra(name, part) : {});
      return { axes, lines, mode, cols: part.cols, crop, source: name };
    });
  }

  // ---------- plain snapshots, styled as the code paints them ----------

  /** Composer frames: rules follow focus (theme.rule), the marker is bold, hints are tertiary. */
  function composer(lines, m, { focused = true } = {}) {
    const rule = focused ? m.accent : m.separator;
    let queue = null;
    return lines.map((line) => {
      if (/^─+$/.test(line)) return `[fg${rule}]${line}`;
      const more = /^── ([↑↓] \d+ more) (─+)$/.exec(line);
      if (more) return `[fg${rule}]──[fg${m.tertiary}] ${more[1]} [fg${rule}]${more[2]}`;
      if (line === "›" || line.startsWith("› ")) return `[b]› [-]${line.slice(2)}`;
      if (line === "  ? for shortcuts") return `[-]  [fg${m.tertiary}]? for shortcuts`;
      const working = /^• (Working)( \(.*\))$/.exec(line);
      if (working) return `[shimmer]•[-] [shimmer]${working[1]}[fg${m.tertiary}]${working[2]}`;
      const title = /^ {2}• (.+)$/.exec(line);
      if (title) {
        queue = title[1].startsWith("Queued") ? "i," : "";
        return `[-]  [fg${m.tertiary}]• [-]${title[1]}`;
      }
      if (queue !== null && (line.startsWith("    ↳ ") || line.startsWith("      "))) {
        const italic = line.includes("edit last queued message") ? "" : queue;
        return `[${italic}fg${m.tertiary}]${line}`;
      }
      return line;
    });
  }

  /** header.rs: separator border, cat logo split Accent / Accent2, bold name, dim details. */
  function header(lines, m) {
    let row = -1;
    const wide = lines[0].length >= 62;
    return lines.map((line) => {
      if (/^[╭╰]/.test(line)) return `[fg${m.separator}]${line}`;
      row += 1;
      const inner = line.slice(2, -2);
      const border = (body) => `[fg${m.separator}]│ ${body}[fg${m.separator}] │`;
      if (wide && row >= 1 && row <= 4) {
        const logo = inner.slice(0, 13);
        const rest = inner.slice(13);
        const left = `[fg${m.accent}]${logo.slice(0, 7)}[fg${m.accent2}]${logo.slice(7)}`;
        return border(left + info(rest, row - 1, m));
      }
      if (!wide && row >= 0 && row <= 2) return border(info(inner, row, m));
      const keys = inner.replace(/(^|\s{3})(\/resume|\/|@|ctrl\+o)(?= )/g, (_, gap, key) => `[-]${gap}[fg${m.tertiary}]${key}[-]`);
      return border(`[-]${keys}`);
    });
  }

  function info(text, row, m) {
    const pad = /^(\s*)(.*?)(\s*)$/.exec(text);
    const [lead, body, tail] = [pad[1], pad[2], pad[3]];
    if (!body) return `[-]${text}`;
    if (row === 0) {
      const [name, version] = body.split(/(?=  v)/);
      return `[-]${lead}[b]${name}[fg${m.tertiary}]${version || ""}[-]${tail}`;
    }
    return `[-]${lead}[fg${row === 1 ? m.secondary : m.tertiary}]${body}[-]${tail}`;
  }

  function boxes(lines) {
    const out = [];
    let box = null;
    for (const line of lines) {
      if (line.startsWith("╭")) box = [];
      if (box) box.push(line);
      if (line.startsWith("╰") && box) {
        out.push(box);
        box = null;
      }
    }
    return out;
  }

  // ---------- authored frames: compaction strings from app.rs and status.rs ----------

  const W = 80;

  function compacting(mode) {
    const m = MIST[mode];
    return [
      `[shimmer]◇[-] [shimmer]Auto-compacting · summarizing · retry 1 · trimmed 2×[fg${m.tertiary}] (14s • esc to cancel)`,
      "",
      ...composer(["─".repeat(W), "›", "─".repeat(W), "  ? for shortcuts"], m),
    ];
  }

  /** The default status line in text and icon form, its context item swapped per state. */
  function statusStates() {
    const lines = DATA.snapshots.status__snapshots_cover_default_all_items_narrow_and_low_context.parts;
    const [text, icons] = ["== default", "== default icons"].map((title) => lines.find((p) => p.title === title).lines[0]);
    const ctx = "[fg#f2b590]";
    return [
      ["会话开始", "Context 100% left", "\uf240 100%"],
      ["检查点后", "Context compacted", "\ueac5 compacted"],
    ].map(([state, item, icon]) => ({
      axes: { 状态: state },
      lines: [text.replace(`${ctx}Context 75% left`, ctx + item), "", icons.replace(/\[fg#f2b590\]\S+ 75%/, ctx + icon)],
      mode: "dark",
      cols: W,
      source: "status__snapshots_cover_default_all_items_narrow_and_low_context",
    }));
  }

  const N = (term, text) => [term, text];

  const PLATES = [
    {
      id: "tokens", no: "00", title: "令牌", en: "Tokens",
      lede: "Mist 色板、表面阶梯、动效节拍与字形词汇。所有画面都由这些常数组合而成。",
      custom: tokens,
    },
    {
      id: "transcript", no: "01", title: "转写", en: "Transcript",
      lede: "一轮对话落入回滚区后的样子。切换主题、列宽与色彩级别，可以看到同一组单元在三级降级链上的形态。",
      exhibits: [{
        title: "单元总览",
        candidates: () => fromSnapshots(["cells__cells_dark_truecolor", "cells__cells_light_truecolor", "cells__cells_narrow", "cells__cells_ansi256", "cells__cells_ansi16_without_palette"]),
        notes: [
          N("›", "用户消息：整块铺 5% 表面色，上下各留一行同色空行；`›` 粗体叠 DIM。"),
          N("• 思考", "DIM 引导符 + Tertiary 斜体；行内代码仍用 Accent，不被斜体吞掉。"),
          N("Explored", "只读工具合并为一组，`└` 下按 Read / Search / List 归并，动词 Cyan。"),
          N("Ran / Called", "成功的 `•` 用 Success 粗体，失败用 Error；耗时 ≥1s 才显示；输出只留最后 3 行，其余折成 `+ N lines`。"),
          N("Updated", "diff 行号 DIM，增删行整行铺底，语法色叠在底色之上。"),
          N("Markdown", "标题粗体下划线；引用是 Separator 竖线 + Secondary 文本；表格边框为前景 20% 混合。"),
          N("Worked for", "回合结束的居中规则线；分钟之后的秒数补零，宽度稳定。"),
          N("■", "错误独立成段，整行 Error 色。"),
        ],
      }],
    },
    {
      id: "tools", no: "02", title: "工具", en: "Tool activity",
      lede: "工具从排队、运行到完成的几种视图：live 区带动画，回滚区收起；全屏下点控制行原地展开单个块，ctrl+o 展开全部。",
      exhibits: [
        {
          title: "运行中",
          candidates: () => fromSnapshots(["cells__tool_states_dark", "cells__tool_states_light"], "== running"),
          notes: [
            N("•", "truecolor 下引导符随 2s 扫光；其他级别在 `•` 与 DIM `◦` 间每 600ms 交替，整周期 1.2s。"),
            N("Exploring", "探索组进行中用现在时；完成后变为 Explored。"),
            N("Calling", "MCP 调用标出 `· pending` / `· running`，Ran 不标。"),
          ],
        },
        {
          title: "展开视图（ctrl+o）",
          candidates: () => fromSnapshots(["cells__tool_states_dark", "cells__tool_states_light"], "== expanded"),
          notes: [
            N("多行命令", "续行在标题下方逐行列出，耗时挂在最后一行。"),
            N("输出", "不再截到 3 行，空行也保留。"),
            N("思考", "标题粗体斜体，正文 DIM 斜体。"),
          ],
        },
        {
          title: "点击展开（全屏）",
          candidates: () => fromSnapshots(["cells__folds_dark", "cells__folds_light"], undefined, {
            extra: (_, part) => {
              const [cell, state] = part.title.slice(3).split(" ");
              return { 单元: { output: "输出", explored: "探索组", checkpoint: "检查点", thinking: "思考" }[cell], 状态: state ? "展开" : "收起" };
            },
          }),
          notes: [
            N("控制行", "折叠的块下方一行 DIM 标签，缩进 4 列；只有点标签才切换，点缩进处无效。"),
            N("+ N lines", "输出被截断时，原有的计数行本身就是控制行，不再另起一行；展开后变为 `− Show less`。"),
            N("悬停", "指针落在标签上时改为 Accent 粗体（“输出 · 收起”一帧即悬停态）。"),
            N("范围", "只在全屏模式；inline 的原生回滚区无法重绘，仍用 ctrl+o。/btw 的思考摘要用同一种控制行，逐条切换。"),
          ],
        },
      ],
    },
    {
      id: "composer", no: "03", title: "输入", en: "Composer",
      lede: "会话头与底部输入区。",
      exhibits: [
        {
          title: "会话头",
          candidates: () => {
            const lines = DATA.snapshots.app__session_header.parts[0].lines;
            return boxes(lines).flatMap((box) => ["dark", "light"].map((mode) => ({
              axes: { 列宽: String([...box[0]].length), 主题: mode === "light" ? "浅" : "深" },
              lines: header(box, MIST[mode]), mode, source: "app__session_header",
            })));
          },
          notes: [
            N("logo", "“tc” 画在 8 像素网格上，用半块字符压成 4 行；左半 Accent、右半 Accent2。"),
            N("边框", "Separator 色圆角框，宽度上限 78 列。"),
            N("窄屏", "不足 60 列时去掉 logo 与快捷键行，只留三行信息。"),
          ],
        },
        {
          title: "聚焦与失焦",
          candidates: () => {
            const lines = parts("app__composer_states_80", "== empty")[0].part.lines;
            return ["dark", "light"].flatMap((mode) => [true, false].map((focused) => ({
              axes: { 状态: focused ? "聚焦" : "失焦", 主题: mode === "light" ? "浅" : "深" },
              lines: composer(lines, MIST[mode], { focused }), mode, cols: 80,
              source: "app__composer_states_80", hollow: !focused,
            })));
          },
          notes: [
            N("规则线", "终端窗口聚焦时两条规则线用 Accent，失焦（FocusLost）退回 Separator。这是现有行为，BTW 侧栏共用同一规则。"),
            N("›", "提示符粗体，不随焦点变色：变色只留给规则线，避免 Accent 面积扩大。"),
            N("? for shortcuts", "Tertiary，只在空闲时出现。"),
          ],
        },
        ...[["多行与换行", "== multi-line", [N("续行", "第二行起缩进两列，与 `› ` 对齐。")]],
          ["滚动提示", "== scroll hints", [N("↑ 1 more", "超出高度时把隐藏行数嵌进规则线，标签用 Tertiary。")]],
          ["排队预览", "== queued previews", [
            N("下一次工具调用后提交", "插话消息 DIM，正常字形。"),
            N("Queued follow-up", "后续输入 DIM 斜体，超过 3 行以 `…` 收尾。"),
            N("alt+up", "提示可编辑最后一条排队消息。"),
          ]]].map(([title, part, notes]) => ({
          title,
          candidates: () => fromSnapshots(["app__composer_states_80", "app__composer_states_40"], part, { color: (lines, m) => composer(lines, m) }),
          notes,
        })),
      ],
    },
    {
      id: "menus", no: "04", title: "补全", en: "Completion",
      lede: "`@` 文件、`$` 技能与插件资源共用一个菜单，最多 8 行，位于输入区上方。",
      exhibits: [
        {
          title: "资源菜单",
          candidates: () => fromSnapshots("resources__resource_and_permissions_snapshots", /^menu /),
          notes: [
            N("Files  All", "首行是当前分组与匹配数，shift+tab 切换分组。"),
            N("›", "选中行粗体 Accent。"),
            N("描述", "Tertiary，放不下时在末尾截断。"),
            N("窄屏", "不足 60 列时去掉右侧的类型列。"),
          ],
        },
        {
          title: "重音折叠匹配",
          candidates: () => fromSnapshots("resources__resource_and_permissions_snapshots", "accent-folded match"),
          notes: [N("Café", "输入 cafe 即可命中；命中的字符逐个加粗。")],
        },
      ],
    },
    {
      id: "prompts", no: "05", title: "审批与提问", en: "Prompts",
      lede: "会话等待用户的两类交互：权限审批与模型提问。同一时刻只排一个。",
      exhibits: [
        {
          title: "权限审批",
          candidates: () => fromSnapshots("app__prompt_layouts", /^\d+x\d+$/),
          notes: [
            N("标题", "标题写明要做的事（Apply these edits? / Run this command?），右端是工具名与默认策略。"),
            N("窄屏", "差异只留开头几行，过长的选项以 `…` 截断，标题右端让位。"),
            N("ctrl+o", "打开完整请求。"),
          ],
        },
        {
          title: "提问",
          candidates: () => fromSnapshots("app__ask_layouts", /^\d+x\d+ light=|^notes$|^confirmation$/, {
            extra: (_, part) => ({ 步骤: /^\d/.test(part.title) ? "选择" : part.title === "notes" ? "备注" : "确认" }),
          }),
          notes: [
            N("(Recommended)", "推荐项写在选项文本里，不额外着色。"),
            N("notes", "tab 为当前选项附加备注，草稿留在本地直到提交。"),
            N("确认", "存在未答问题时，提交前再确认一次。"),
          ],
        },
        {
          title: "提问记录",
          candidates: () => fromSnapshots("app__ask_records_render_answers_notes_and_argument_fallbacks", undefined, {
            extra: (_, part) => ({ 结果: { submitted: "已提交", cancelled: "已取消", error: "出错" }[part.title.split(" ").pop()] }),
          }),
          notes: [N("记录", "回答与备注写回转写区，取消和出错各有独立形态。")],
        },
      ],
    },
    {
      id: "session", no: "06", title: "会话导航", en: "Resume & rewind",
      lede: "恢复旧会话与回退到某条消息，都在备用屏上完成。",
      exhibits: [
        {
          title: "恢复会话",
          candidates: () => fromSnapshots("app__resume_layouts", undefined, {
            extra: (_, part) => ({ 布局: { "100x16": "对话", "100x12": "详情", "40x9": "窄屏" }[part.title] }),
          }),
          notes: [
            N("›", "选中行。"),
            N("This folder  All", "当前的筛选与排序项加粗，获得焦点的一组用 Accent；tab 切换焦点，←→ 改值。"),
            N("│", "列表下方是选中会话的对话末尾；ctrl+e 加上会话详情，ctrl+t 打开完整转写。"),
            N("1 / 2", "页码贴在底部提示行右端。"),
          ],
        },
        {
          title: "回退：消息",
          candidates: () => fromSnapshots("rewind__rewind_Messages"),
          notes: [
            N("›", "选中行粗体 Accent，最近一条标 latest。"),
            N("+24 −6", "增删统计分用 Success / Error，减号是 U+2212。"),
          ],
        },
        {
          title: "回退：动作",
          candidates: () => fromSnapshots("rewind__rewind_Actions"),
          notes: [
            N("动作", "恢复代码、对话或两者；不可用的项给出原因。"),
            N("line counts unavailable", "无法计数的文件逐个注明，不算进标题右端的统计。"),
          ],
        },
      ],
    },
    {
      id: "status", no: "07", title: "状态行", en: "Status line",
      lede: "输入区下方一行，只显示真实值。颜色取自语法主题的 scope 色并做软化。",
      exhibits: [
        {
          title: "默认条目",
          candidates: () => fromSnapshots("status__snapshots_cover_default_all_items_narrow_and_low_context", /^== (default|default icons|low context warning|low context error|all items narrow)$/, {
            extra: (_, part) => ({ 形态: { "== default": "文本", "== default icons": "图标", "== low context warning": "余量 15%", "== low context error": "余量 5%", "== all items narrow": "窄屏" }[part.title] }),
          }),
          notes: [
            N(" · ", "分隔符 Tertiary。"),
            N("图标", "Nerd Font 形态；电池按 25% 分档，满格只给 100%。"),
            N("告警", "余量 ≤20% 电池变 Warning，≤10% 变 Error；run-state 出错同样变色。"),
            N("窄屏", "放不下时整项截断加 `…`，不拆半个条目。"),
          ],
        },
        {
          title: "全部条目",
          candidates: () => fromSnapshots("status__snapshots_cover_default_all_items_narrow_and_low_context", "== all items"),
          notes: [N("左列", "条目名；右列依次是文本与图标两种形态。")],
        },
        {
          title: "/statusline 设置",
          candidates: () => fromSnapshots("status__picker_layout_fits_short_and_narrow_terminals"),
          notes: [N("预览", "勾选、排序与样式即时预览。")],
        },
      ],
    },
    {
      id: "context", no: "08", title: "上下文", en: "/context",
      lede: "冻结的上下文浏览器：类别占比、系统提示、工具、注入、消息与缓存诊断。",
      exhibits: [{
        title: "上下文面板",
        candidates: () => fromSnapshots(["context__context_Overview", "context__context_System", "context__context_Tools", "context__context_Injections", "context__context_Messages", "context__context_Cache", "context__context_compaction_cache"], undefined, {
          extra: (name, part) => ({
            页签: part.title.startsWith("Definition") ? "Tools · 定义" : name === "context__context_compaction_cache" ? "Cache · 压缩" : name.replace("context__context_", ""),
          }),
        }),
        notes: [
          N("■ ◧ □", "16 列网格，每格约等于窗口的 1/256：类别 / 混合 / 空闲（估计）。"),
          N("· · ·", "图例用点线引向右对齐的数值与百分比。"),
          N("Cache · 压缩", "压缩相关的未命中单独归因，并注明“估计，不是账单或因果证明”。"),
        ],
      }],
    },
    {
      id: "side", no: "09", title: "并行能力", en: "Goal · loop · BTW · plugins · subagents",
      lede: "目标、定时任务、旁路提问、插件面板与子代理：都复用转写单元与输入区，不另起一套样式。",
      exhibits: [
        {
          title: "目标",
          candidates: () => fromSnapshots("goal__goal_summary_status"),
          notes: [N("Pursuing goal", "目标进行中时显示在规则线下方，带已用 token 与时长。")],
        },
        {
          title: "定时任务",
          candidates: () => fromSnapshots("scheduled__scheduled_tasks"),
          notes: [
            N("/loop", "先立即执行一次，再按间隔在空闲时重复；7m 这类无法均匀落在时钟上的间隔取最近的 cron 周期，并在确认里写明。"),
            N("/cron-list", "会话内任务与项目持久任务一起列出，下次时间带本地时区偏移；运行中也能查看和取消。"),
            N("scheduled tasks", "有任务时显示在规则线下方：数量与下一次时间。输入框里有草稿时，到期的任务等你发送或清空后再跑。"),
          ],
        },
        {
          title: "BTW 旁路提问",
          candidates: () => fromSnapshots("btw__side_panel"),
          notes: [
            N("侧栏", "独立的输入区与转写，主会话的提问与审批在后台排队。"),
            N("main needs input", "主会话等待时在底部提示，ctrl+/ 切回。"),
          ],
        },
        {
          title: "插件面板",
          candidates: () => fromSnapshots("plugins__plugin_panel_status"),
          notes: [N("声明式", "插件只能提交 text / markdown / list / form / progress 描述树，由 TUI 绘制。")],
        },
        {
          title: "子代理",
          candidates: () => fromSnapshots("subagents__subagents", undefined, {
            extra: (_, part) => ({ 视图: { waiting: "等待中", finished: "已完成", viewer: "浏览器", output: "输出" }[part.title.split(" / ").pop()] }),
          }),
          notes: [
            N("↑120 ↓30", "输入 / 输出 token；R / W 为缓存读写。"),
            N("浏览器", "左列任务、右列详情；^F 搜索、^S 分组、^T 显示思考。"),
          ],
        },
        {
          title: "权限总览",
          candidates: () => fromSnapshots("resources__resource_and_permissions_snapshots", /^permissions /),
          notes: [N("规则", "后出现的匹配优先；会话授权在 reload 后清空。")],
        },
      ],
    },
    {
      id: "theme", no: "10", title: "语法主题", en: "Theme picker",
      lede: "/theme 选择语法主题：方向键实时预览，Enter 保存，Esc 还原。代码、diff、表头与状态行共用 scope 色。",
      exhibits: [{
        title: "主题选择器",
        candidates: () => fromSnapshots(["viewport__theme_picker_80_dark", "viewport__theme_picker_80_light", "viewport__theme_picker_40_dark", "viewport__theme_picker_40_light"]),
        notes: [N("automatic", "保持 Catppuccin Mocha / Latte 默认，随终端背景切换。")],
      }],
    },
    {
      id: "compaction", no: "11", title: "上下文压缩", en: "Compaction",
      lede: "长会话不必重开。原生检查点（`/compact`、阈值、溢出）把早先的对话总结成摘要；模型也可以用四个历史工具按区间压缩，原文随时可检索。两条路径共用 ◇ 这一套呈现。",
      exhibits: [
        {
          title: "进行中",
          candidates: () => ["dark", "light"].map((mode) => ({ axes: { 主题: mode === "light" ? "浅" : "深" }, lines: compacting(mode), mode, cols: W, source: "app.rs" })),
          notes: [
            N("◇", "与 Working 同样扫光；计时从压缩开始算起，esc 取消。"),
            N("retry · trimmed", "摘要请求的重试与输入裁剪次数并入标签；窄屏时标签先让位，计时与 esc 提示保留。"),
            N("没有进度条", "摘要请求的输出长度未知，阶段耗时也极不均匀，文字阶段足够。"),
          ],
        },
        {
          title: "检查点与维护",
          candidates: () => fromSnapshots(["cells__context_maintenance_dark", "cells__context_maintenance_ansi16_narrow"], undefined, {
            extra: (_, part) => ({ 状态: part.title.slice(3) }),
          }),
          notes: [
            N("◇", "模型内部活动，与 `•` 的外部动作区分；完成后保持 Tertiary，不用 Success。"),
            N("Maintained context", "相邻的四个历史工具合并为一组，每次调用一行；参数与摘要只在展开视图出现。"),
            N("deferred", "冷却期的拒绝是预期结果，DIM；真正失败时 ◇ 与错误码才用 Error。"),
            N("◆ / ▲", "降级的检查点与自动压缩暂停用 Warning：是状态，不是失败。"),
          ],
        },
        {
          title: "状态行",
          candidates: statusStates,
          notes: [
            N("100%", "会话的第一个请求之前，上下文项是满格。"),
            N("compacted", "检查点之后、首个新用量之前，上下文项写明 compacted，不显示空电池。"),
            N("Compacting", "启用 run-state 项时，压缩期间显示该状态。"),
          ],
        },
      ],
    },
  ];

  // ---------- DOM ----------

  const el = (tag, cls, text) => {
    const node = document.createElement(tag);
    if (cls) node.className = cls;
    if (text !== undefined) node.textContent = text;
    return node;
  };

  /** `code` spans inside note text. */
  function rich(text) {
    const frag = document.createDocumentFragment();
    text.split(/(`[^`]+`)/).forEach((chunk) => {
      if (chunk.startsWith("`") && chunk.endsWith("`") && chunk.length > 1) frag.append(el("code", "", chunk.slice(1, -1)));
      else frag.append(chunk);
    });
    return frag;
  }

  function frameView({ lines, mode, cols, crop, hollow }) {
    let frame = Term.parse(lines);
    if (crop) {
      frame = {
        rows: frame.rows.slice(crop.from || 0, crop.to).map((row) => {
          let width = 0;
          return row.filter((cell) => (width += cell.w) <= crop.cols);
        }),
        cursor: null,
      };
    }
    const term = el("div", `term ${mode}`);
    Term.render(term, frame, { mode, cols: crop ? crop.cols : cols });
    if (hollow) term.classList.add("blur");
    return { term, frame };
  }

  function segmented(values, active, onPick) {
    const group = el("div", "seg");
    for (const value of values) {
      const button = el("button", value === active ? "on" : "", value);
      button.type = "button";
      button.addEventListener("click", () => onPick(value));
      group.append(button);
    }
    return group;
  }

  /** The candidate matching most of `state`; the axis just picked outweighs the rest. */
  function choose(candidates, state, priority) {
    let best = candidates[0];
    let score = -1;
    for (const c of candidates) {
      const s = Object.entries(state).reduce((sum, [k, v]) => sum + (c.axes[k] === v ? (k === priority ? 10 : 1) : 0), 0);
      if (s > score) {
        best = c;
        score = s;
      }
    }
    return best;
  }

  function exhibit(spec) {
    const fig = el("figure", "ex");
    const head = el("div", "ex-head");
    head.append(el("h3", "", spec.title));
    const controls = el("div", "ctl");
    head.append(controls);
    const body = el("div", "ex-body");
    const stage = el("div", "stage");
    body.append(stage);
    if (spec.notes) {
      const dl = el("dl", "notes");
      for (const [term, text] of spec.notes) {
        dl.append(el("dt", "", term));
        const dd = el("dd");
        dd.append(rich(text));
        dl.append(dd);
      }
      body.append(dl);
    }
    fig.append(head, body);

    let candidates = null;
    const state = {};
    const entry = { fig, rendered: false, draw, reset };
    let priority = null;

    function axes() {
      const map = {};
      for (const c of candidates) for (const [k, v] of Object.entries(c.axes)) (map[k] ||= []).includes(v) || map[k].push(v);
      if (map.列宽) map.列宽.sort((a, b) => b - a);
      return Object.entries(map).filter(([, v]) => v.length > 1);
    }

    function reset(axis) {
      candidates ||= spec.candidates();
      priority = axis || null;
      for (const [axis, values] of axes()) {
        state[axis] = values.includes(prefs[axis]) ? prefs[axis] : state[axis] && values.includes(state[axis]) ? state[axis] : values[0];
      }
    }

    function draw() {
      candidates ||= spec.candidates();
      if (!candidates.length) {
        stage.replaceChildren(el("p", "missing", "快照中没有对应的画面。"));
        return;
      }
      if (!entry.rendered) reset();
      entry.rendered = true;
      const pick = choose(candidates, state, priority);
      Object.assign(state, Object.fromEntries(Object.keys(state).map((k) => [k, pick.axes[k]])));
      controls.replaceChildren(...axes().map(([axis, values]) => {
        const wrap = el("div", "ctl-axis");
        wrap.append(el("span", "ctl-label", axis), segmented(values, state[axis], (value) => {
          state[axis] = value;
          priority = axis;
          draw();
        }));
        return wrap;
      }));
      const { term, frame } = frameView(pick);
      const shell = el("div", "frame");
      const bar = el("div", "bar");
      const meta = [pick.cols || Math.max(...frame.rows.map((r) => r.reduce((s, c) => s + c.w, 0))) + " cols", pick.mode === "light" ? "浅色" : "深色", pick.axes.色彩].filter(Boolean).join(" · ");
      bar.append(el("span", "src", pick.source));
      bar.append(el("span", "meta", meta), copyButton(() => Term.text(frame)));
      shell.append(bar, term);
      stage.replaceChildren(shell);
    }

    exhibits.push(entry);
    return fig;
  }

  function copyButton(read) {
    const button = el("button", "copy", "复制");
    button.type = "button";
    button.addEventListener("click", async () => {
      try {
        await navigator.clipboard.writeText(read());
        button.textContent = "✓ 已复制";
        button.classList.add("done");
      } catch {
        button.textContent = "复制失败";
      }
      setTimeout(() => {
        button.textContent = "复制";
        button.classList.remove("done");
      }, 2000);
    });
    return button;
  }

  // ---------- tokens plate ----------

  function tokens(root) {
    const ROLES = {
      text: ["Text", "正文"], secondary: ["Secondary", "次级文本、会话模型名"], tertiary: ["Tertiary", "DIM 角色：提示、耗时、树枝"],
      separator: ["Separator", "规则线（失焦）、引用竖线、边框"], accent: ["Accent", "交互层：聚焦线、选中项、链接"],
      accent2: ["Accent2", "生命层：只出现在正在发生的状态"], success: ["Success", "完成的工具"], warning: ["Warning", "余量告警、降级检查点与暂停"], error: ["Error", "失败、错误段落"],
    };
    const palette = el("div", "token-block");
    palette.append(el("h3", "", "Mist 色板"), el("p", "token-note", "palette.rs mist_rgb：同一语义在深浅背景下各取一值；256 色取 CIE76 最近色，16 色退回终端自身的 ANSI 槽位，中性色改用 DIM。"));
    const grid = el("div", "swatches");
    for (const [key, [name, role]] of Object.entries(ROLES)) {
      const card = el("div", "swatch");
      const chips = el("div", "chips");
      for (const mode of ["dark", "light"]) {
        const chip = el("div", `chip ${mode}`);
        chip.style.setProperty("--c", MIST[mode][key]);
        chip.append(el("span", "", MIST[mode][key]));
        chips.append(chip);
      }
      card.append(chips, el("b", "", name), el("small", "", role));
      grid.append(card);
    }
    palette.append(grid);

    const fx = Term.FIXTURE.dark;
    const ladder = el("div", "token-block");
    ladder.append(el("h3", "", "表面与边框"), el("p", "token-note", "在测试夹具背景 #1e1e1e 上按 palette.rs 的 blend 计算。"));
    const steps = el("div", "ladder");
    const rows = [
      ["surface", "5%", Term.blend("#ffffff", fx.bg, 0.05), "fill", "用户消息底、选中行"],
      ["border", "20%", Term.blend(fx.fg, fx.bg, 0.2), "line", "表格边框"],
    ];
    for (const [name, alpha, color, kind, use] of rows) {
      const row = el("div", "step");
      const sample = el("div", `sample ${kind}`);
      sample.style.setProperty("--c", color);
      sample.style.setProperty("--bg", fx.bg);
      row.append(sample, el("code", "", name), el("span", "alpha", alpha), el("span", "hex", color), el("span", "use", use));
      steps.append(row);
    }
    ladder.append(steps);

    const motion = el("div", "token-block");
    motion.append(el("h3", "", "等待信号"), el("p", "token-note", "动画节拍沿用 100ms tick。闪烁整周期不得快于 1.2s；完成即退场，稳态内容上不留常亮动画。"));
    const signals = el("div", "signals");
    const sig = (label, period, note, lines) => {
      const card = el("div", "signal");
      const { term } = frameView({ lines, mode: "dark", cols: 24 });
      card.append(term, el("b", "", label), el("span", "period", period), el("small", "", note));
      signals.append(card);
      return term;
    };
    sig("扫光", "2.0s", "live 文本：Working、思考标题、运行中的工具", ["[shimmer]•[-] [shimmer]Working[fg#666c77] (12s • esc…"]);
    const blinker = sig("交替", "1.2s", "非 truecolor 的引导符：• 与 DIM ◦ 每 600ms 交替", ["•[-] Working[fg#666c77] (12s • esc…"]);
    motion.append(signals);
    let on = true;
    setInterval(() => {
      on = !on;
      const first = blinker.querySelector(".ln span");
      if (!first) return;
      first.textContent = on ? "•" : "◦";
      first.style.color = on ? "" : Term.blend(fx.fg, fx.bg, 0.5);
    }, 600);

    const timing = el("div", "token-block");
    timing.append(el("h3", "", "时长"), el("p", "token-note", "终端没有缓动帧，时长表现为停留时间。"));
    const bars = el("div", "timing");
    const max = 2000;
    for (const [name, ms, use] of [
      ["tick", 100, "动画节拍"], ["escape-pending", 500, "Esc 双击窗口"], ["blink", 1200, "交替引导符整周期"], ["shimmer", 2000, "扫光周期"],
    ]) {
      const row = el("div", "bar-row");
      const track = el("div", "track");
      const fillBar = el("div", "track-fill");
      fillBar.style.width = `${(ms / max) * 100}%`;
      track.append(fillBar);
      row.append(el("code", "", name), track, el("span", "ms", `${ms}ms`), el("span", "use", use));
      bars.append(row);
    }
    timing.append(bars);

    const glyphs = el("div", "token-block");
    glyphs.append(el("h3", "", "字形词汇"), el("p", "token-note", "每个字形只承担一种意思。"));
    const table = el("div", "glyphs");
    for (const [g, meaning] of [
      ["•", "条目引导符：消息、工具、动作"], ["◦", "非 truecolor 下的交替帧"], ["└", "工具输出树枝"],
      ["›", "输入提示符、选中项"], ["❯", "resume 选中行"], ["↳", "排队中的消息"],
      ["■", "错误段落；/context 中的类别格"], ["◧", "/context 混合格"], ["□", "/context 空闲格"],
      ["─", "规则线；回合结束线"], ["…", "截断：末尾或中间"], ["·", "同一行内的分隔"],
      ["◇", "模型内部活动：上下文维护、检查点"], ["◆", "降级的检查点"], ["▲", "可恢复的暂停"],
      ["+ −", "控制行：点击展开与收起"],
    ]) {
      const cell = el("div", "glyph");
      cell.append(el("span", "g", g), el("span", "m", meaning));
      table.append(cell);
    }
    glyphs.append(table);

    const chain = el("div", "token-block");
    chain.append(el("h3", "", "降级链"), el("p", "token-note", "同一份转写单元的前 22 行、前 44 列，分别取自三级色彩的快照。每个视觉决策都要同时给出这三种形态。"));
    const trio = el("div", "trio");
    for (const [name, label] of [["cells__cells_dark_truecolor", "TrueColor · 精确 RGB"], ["cells__cells_ansi256", "256 色 · CIE76 最近色"], ["cells__cells_ansi16_without_palette", "16 色 · 语义槽位 + DIM"]]) {
      const part = DATA.snapshots[name].parts[0];
      const box = el("figure", "mini");
      const { term } = frameView({ lines: part.lines, mode: "dark", crop: { from: 0, to: 22, cols: 44 } });
      box.append(term, el("figcaption", "", label));
      trio.append(box);
    }
    chain.append(trio);

    root.append(palette, ladder, motion, timing, glyphs, chain);
  }

  // ---------- page ----------

  function build() {
    const plates = document.getElementById("plates");
    const nav = document.getElementById("nav");
    for (const plate of PLATES) {
      const section = el("section", "plate");
      section.id = plate.id;
      const head = el("header", "plate-head");
      const titles = el("div", "plate-titles");
      const en = el("p", "en");
      en.append(el("b", "", plate.no), plate.en);
      titles.append(en, el("h2", "", plate.title));
      const lede = el("p", "lede");
      lede.append(rich(plate.lede));
      head.append(titles, lede);
      section.append(head);
      const body = el("div", "plate-body");
      if (plate.custom) plate.custom(body);
      else for (const spec of plate.exhibits) body.append(exhibit(spec));
      section.append(body);
      plates.append(section);

      const link = el("a");
      link.href = `#${plate.id}`;
      link.append(el("span", "nav-no", plate.no), el("span", "nav-title", plate.title));
      nav.append(link);
    }

    const pending = new IntersectionObserver((entries) => {
      for (const entry of entries) {
        if (!entry.isIntersecting) continue;
        const target = exhibits.find((e) => e.fig === entry.target);
        if (target && !target.rendered) target.draw();
        pending.unobserve(entry.target);
      }
    }, { rootMargin: "600px 0px" });
    exhibits.filter((e) => !e.rendered).forEach((e) => pending.observe(e.fig));

    const spy = new IntersectionObserver((entries) => {
      for (const entry of entries) {
        if (!entry.isIntersecting) continue;
        nav.querySelectorAll("a").forEach((a) => a.classList.toggle("on", a.hash === `#${entry.target.id}`));
      }
    }, { rootMargin: "-30% 0px -60% 0px" });
    document.querySelectorAll(".plate").forEach((p) => spy.observe(p));

    const reveal = new IntersectionObserver((entries) => {
      for (const entry of entries) if (entry.isIntersecting) entry.target.classList.add("in");
    }, { rootMargin: "0px 0px -10% 0px" });
    document.querySelectorAll(".plate-head, .ex, .token-block").forEach((n) => reveal.observe(n));

    for (const axis of ["主题", "列宽"]) {
      const holder = document.querySelector(`[data-pref="${axis}"]`);
      const paint = () => holder.replaceChildren(segmented(axis === "主题" ? ["深", "浅"] : ["80", "40"], prefs[axis], (value) => {
        prefs[axis] = value;
        paint();
        for (const e of exhibits) {
          if (!e.rendered) continue;
          e.reset(axis);
          e.draw();
        }
      }));
      paint();
    }

    const hero = document.getElementById("hero-frame");
    const box = boxes(DATA.snapshots.app__session_header.parts[0].lines)[0];
    hero.append(frameView({ lines: header(box, MIST.dark), mode: "dark", cols: [...box[0]].length }).term);

    for (const cmd of document.querySelectorAll(".install .cmd")) {
      const code = cmd.querySelector("code");
      cmd.append(copyButton(() => code.textContent));
    }

    const count = Object.keys(DATA.snapshots).length;
    const frames = Object.values(DATA.snapshots).reduce((sum, s) => sum + s.parts.length, 0);
    document.getElementById("stamp").textContent = `${count} 份快照 · ${frames} 幅画面 · 快照提交 ${DATA.commit} · ${DATA.date}`;
  }

  build();
})();
