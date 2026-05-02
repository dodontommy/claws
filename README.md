# claws

A TUI multiplexer for Claude Code sessions. Run multiple `claude` instances at once, switch between them, and watch their state from a single terminal.

## What it does

- Owns a daemon that spawns `claude` processes inside PTYs
- Dashboard view: cards per session showing status (idle / streaming / awaiting permission / exited), Claude's auto-generated title, last message, model, token usage, working directory
- Attached view: full PTY rendering via `vt100`, `Ctrl-Space` prefix for `d`etach / `n`ext / `p`rev / `1`-`9` jump
- Hooks injected per-session for real-time status (`PermissionRequest`, `Stop`, `PreToolUse`, etc.)
- JSONL transcript tail for last message / model / tokens / Claude's auto-title
- SQLite persistence — sessions resume automatically after daemon restart via `claude --resume`
- `c` opens a spawn form: working directory, free-form `claude` flags (`--dangerously-skip-permissions`, `--system-prompt`, etc.), `Ctrl-Y` toggles yolo mode
- Mouse on the dashboard, `r` rename, `R` restart, `i` details popup, `/` filter, `?` keymap

## Install

Once releases are cut, the installers will be on the [Releases](https://github.com/dodontommy/multi-claude/releases) page.

From source:

```
cargo install --locked --git https://github.com/dodontommy/multi-claude
```

## Requires

- A working `claude` CLI on `$PATH` (Claude Code 2.x).

## Why "claws"

Multiple Claude sessions in a single terminal — many claws at once. Also the rust mascot is a crab.
