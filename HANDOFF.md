# Handoff: Command Autocomplete for TUI

## Summary

Implement a command completion feature in the `comping` TUI: pressing `/` at the start of an empty input line opens a popup showing all available slash commands. The user can navigate with ↑↓, select with Enter, or dismiss with Esc.

## Status

**Incomplete.** The core logic is implemented and compiles, but the UI rendering is NOT done. The feature is non-functional as-is — pressing `/` triggers no visible output because the popup is never rendered.

## Files Changed

### `crates/tui/src/app.rs`
- Added `Completion` struct with `commands: Vec<CompletionItem>` and `selected: usize`
- Added `CompletionItem` struct with `name: &'static str` and `usage: &'static str`
- Added `completion: Option<Completion>` field to `Input`
- Added methods to `Input`:
  - `begin_completion()` — populates from `prompt::Command::ALL`, resets selection to 0
  - `cancel_completion()` — clears the popup
  - `completion_up()` — decrement selection
  - `completion_down()` — increment selection
  - `completion_select()` — fills selected command into buffer, e.g. `/help/`

### `crates/tui/src/main.rs`
- Esc key: dismiss completion popup if open
- Enter key: select highlighted command if completion is open
- ↑ key: navigate up in completion selection if open
- ↓ key: navigate down in completion selection if open
- `/`: trigger `begin_completion()` when input is empty and no completion active

## What's Missing (DO NOT SKIP)

### 1. UI Rendering (`crates/tui/src/ui.rs`)
- Must add `render_completion(frame, app, area)` function that draws a popup
- Must modify `render()` to allocate layout space for the popup when `app.input.completion.is_some()`
- Popup currently never renders — pressing `/` does nothing visible

### 2. `completion_select()` formatting
- Currently fills buffer with `/<name>/` (e.g. `/help/`)
- Should likely be `/<name>` without trailing slash, and move cursor to end

## Suggested Skills for Next Agent

- `code-review` — review completed work before final commit
- `diagnosing-bugs` — debug rendering issues if popup doesn't display correctly
- `tdd` — if writing tests, use TDD approach

## Notes

- The repo has 44 uncommitted files modified — most are unrelated changes (Chinese→English comment translations, etc.). Only the `app.rs` and `main.rs` changes listed above are from this session.
- Code compiles with 2 dead-code warnings (will resolve after UI rendering is added)
```
