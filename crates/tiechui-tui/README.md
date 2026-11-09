# Terminal input

The main view owns the alternate screen by default and shows a fullscreen
transcript. The composer and interactions stay at the bottom; the conversation
scrolls with the mouse wheel or PageUp / PageDown. Ctrl+Home / Ctrl+End jump to
the beginning / latest output. Scrolling up
pauses following; Esc returns to the latest output. Drag, double-click and
triple-click select transcript text and copy on mouse release through OSC 52.
Selection freezes the painted transcript while the composer remains live.
Closing a viewer retains the fullscreen chat; exit and suspension restore the
shell screen. Finished cells are cached and resize rewraps their source.

`chui --no-alt-screen` keeps the inline mode, with native terminal scrolling and
selection. Inline resize replay waits 75 ms and retains at most 1,000 rows by
default (VS Code: 1,000; Windows Terminal: 9,001; WezTerm: 3,500; Alacritty:
10,000), with a final repair when a resized stream finishes. Late OSC
foreground/background replies update the palette without entering the composer.

Ctrl+O opens the expanded transcript on the alternate screen, including full
tool output and thinking. Viewers support wheel scrolling, drag selection,
double-click words, triple-click lines and arrow-key selection adjustment.
Non-blank selections copy once on mouse release and keep their highlight.
Arrow-key adjustments do not copy automatically; Enter, Ctrl+C or Super+C
copies explicitly. A plain click keeps its normal action, and Ctrl+C without a
selection keeps its interrupt behavior. Esc clears selection before closing the
viewer. Scrolling, focus loss, resize, closing a viewer and switching sessions
clear selection and discard any pending copy.

`tui.copy_on_select` defaults to `true`. Set it to `false` in `[tui]` to keep only
explicit copy shortcuts; `/reload` applies changes. It covers fullscreen chat and
application text viewers, including viewers opened from inline mode. Inline chat
continues to use the terminal's native selection.

Copy sends the rendered text as UTF-8 through OSC 52 to the terminal, including
over SSH. The footer shows `✓ Sent N chars to clipboard` for about two seconds or
until the next key; the terminal does not confirm that its clipboard accepted the
request. Payloads over 100,000 UTF-8 bytes are neither truncated nor sent: the
footer shows `Selection too long to copy (N chars)` and keeps the selection.

Inside tmux (`TMUX` non-empty), copy sends plain OSC 52 followed by a DCS tmux
passthrough sequence. Enable either `set -g set-clipboard on` with OSC 52 support
for the outer terminal, or `set -g allow-passthrough on` in tmux. Both paths may
be disabled in the default configuration. The outer terminal must also permit
OSC 52 clipboard writes. Enabling both paths can deliver the same text twice.
Tiechui does not change tmux settings or invoke `tmux load-buffer`.

In fullscreen chat, click `+ N lines`, `+ Show details`, or `+ Show summary` to
expand that entry in place. The label and two columns on either side form one
click target, clipped to the terminal width; hovering anywhere in that target
highlights the label. The collapse controls and BTW thinking controls use the
same target. Inline mode uses Ctrl+O for details.

`/theme` previews bundled syntax themes and custom `.tmTheme` files in `themes/`
beside the user config. Typing filters; arrows preview; Enter saves `tui.theme`;
Esc or Ctrl+C restores the original theme. Automatic retains the existing
Catppuccin Mocha/Latte defaults. Explicit themes share scope colors across code,
diffs and table headers; the status line keeps terminal palette colors. `/reload`
reads the saved preference and custom files again.

Committed cells are laid out when appended or invalidated, and pager frames
clone only visible rows. `/context` message lists index filtering once and
format only visible rows. The offline `long_session_visible_frames_stay_within_fixed_budget`
test exercises 20,000 messages with a two-second steady-state frame budget in
`just check`.

Rewind's list and preview use the serialized interaction queue, alongside Ask,
permissions, elicitation and Goal. BTW defers main interactions while its panel
is focused. The TUI receives protocol rows and checkpoint
statistics; file tracking and restore policy belong to the rewind module.

BTW owns a separate in-memory composer and transcript. While its panel is in the
foreground, main Ask and approvals wait; Ctrl+/ returns to them without losing the
side draft or stream. Esc stops only the side reply; empty Ctrl+C/D discards it.

Ctrl+/ uses the kitty protocol or legacy `0x1f` (Ctrl+7). BTW does not enable
xterm modifyOtherKeys: crossterm cannot decode its CSI 27 form, which tmux may
send even when CSI-u format is requested. Support is left to terminal input work.

Escape priority, from the currently focused surface to the idle composer:

| State | Escape behavior |
| --- | --- |
| Ask, permission, elicitation, context, completion, or settings panel | The panel handles the key; it does not arm composer rewind. |
| Rewind preview or checkpoint check | Return to the message list; late results cannot confirm. |
| Rewind list | Close without changing files or conversation. |
| Running model/tool, including Bash | Interrupt with the existing steering/follow-up queue rules; this press does not arm rewind. |
| Idle Bash draft | Clear the editor and exit Bash mode; this press does not arm rewind. |
| Idle focused composer with a draft | First Esc shows `esc again to clear input`; another within 500 ms clears text, pastes, and attachments without saving a draft or history. |
| Idle focused empty composer | First Esc shows `esc again to rewind`; another within 500 ms opens `/rewind`. |

Other input, paste, focus loss, session changes, and panels reset the gesture.
Recognized key releases and Escape repeats do not trigger it. Rewind must be
registered for its gesture; disabling the module retains the previous composer
clear behavior. Running tasks require two fresh Escape presses after stopping.

`@` shows immediate children of the current directory, then fuzzy-searches paths
as you type. Tab enters directories; Enter inserts an atomic `@path` reference.
Shift+Tab switches Files/All, exposing plugins, skills and current-branch tasks;
left/right move the cursor. `$` selects skills by name or description, including
manual-only skills, and inserts a reference bound to the loaded path. `/skills`
opens the same picker through a module event, restoring the draft on cancellation.
Skills load on submission; file selection does not read contents or attach images.
Commands and resources share a menu above the composer, with up to eight rows and
source/type labels that move to the status line below 60 columns. Catalogs arrive through actor
protocol events. File discovery uses `ignore::WalkBuilder` on background threads
and streams paths into a reusable `nucleo` index. Hidden entries are included,
ignore files are respected even outside Git, `.git` is skipped, and directory
symlinks are not followed. Query changes and reopening the menu reuse the index;
changing roots, completing a turn, or reloading invalidates it and cancels its
walker. Matching runs off the terminal thread with bounded ticks and grapheme-aware
highlights. Traversal/path errors and limits are visible (100,000 paths, 16 MiB, 15 s).

Ctrl+V reads clipboard images using platform commands (osascript/pbpaste on
macOS, wl-paste/xclip on Linux). PNG, JPEG, GIF and WebP paths, including file
URLs, become numbered attachments up to 20 MiB. Shell drafts retain path text.
Late clipboard results are discarded after focus, cursor, draft or session changes.
`/status` reports module states and config paths even with optional modules off;
`/permissions` shows rules, temporary grants and this instance's recent decisions
through the shared foreground interaction queue.
