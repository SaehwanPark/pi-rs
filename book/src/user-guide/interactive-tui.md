# Interactive TUI

`pi-rs interactive` provides a high-efficiency terminal interface inspired by Pi, built natively with `ratatui` and `crossterm`.

```bash
pi-rs interactive --config config.json
```

![pi-rs Interactive TUI](../assets/screenshots/interactive-tui.png)

---

## TUI Architecture & Layout

The terminal interface consists of three distinct visual zones:

1. **Transcript View (Scrollable History)**:
   - Displays attributed conversation turns, model reasoning with explicit provenance labels, and tool execution summaries.
   - Distinct semantic styling for operations, paths, diffs, and failures.
2. **Bottom Statusline**:
   - Single-line persistent snapshot:
     ```text
     qwen2.5-coder · idle · turn 4 · 1.2k tokens
     ```
   - Segment roles: active model name, activity (`idle` vs `working`), current turn counter, cumulative token usage.
   - Automatically collapses or truncates cleanly on narrow terminal windows down to a minimum column width without wrapping or crashing.
3. **Editor Buffer**:
   - Multi-line text entry with cursor navigation, word-wrapping, and slash-command completions.

---

## Keybindings & Shortcuts

| Key / Shortcut | Action |
| :--- | :--- |
| `Enter` | Submit current prompt to the agent |
| `Shift-Enter` / `Ctrl-J` | Insert newline in multi-line buffer |
| `Left` / `Right` | Move cursor left/right by character |
| `Up` / `Down` | Move cursor up/down across lines in buffer |
| `Home` / `End` | Jump to start / end of current line |
| `Backspace` / `Delete` | Delete character before / under cursor |
| `Ctrl-W` | Delete word backwards |
| `Ctrl-U` | Clear line backwards to cursor |
| `Tab` | Cycle slash-command completions (`/help`, `/skills`, etc.) |
| `Ctrl-C` | Cancel in-flight model response or interrupt running tool |
| `Ctrl-D` / `/exit` | Exit interactive session |

---

## Semantic Visual Hierarchy

`pi-rs` avoids distracting, decorative UI chrome in favor of high-signal semantic color coding:

- **Reasoning**: Dim italic or bordered block tagged with explicit provenance (e.g. `[native reasoning]`).
- **Tool Requests**: Cyan action indicator showing tool name (`read`, `write`, `edit`, `exec`) and canonicalized path.
- **Mutating Operations**: Highlighted warning badge for mutating operations (`write`, `edit`, `exec`).
- **Success / Failure**: Green checkmarks for successful tool completions; prominent red callouts for failures and refusals.
- **Monochrome Support**: Full fidelity in non-color terminals without escape sequence bleeding.
