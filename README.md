# claws

**The terminal where all your Claude Code sessions live.**

One window, every session, switch between them with a keystroke. No more tmux pane archaeology trying to find the one that's been waiting on a permission prompt for 15 minutes.

![claws dashboard](docs/screenshot.png)

## Why

`claude` is great. Running four of them in different tmux panes across three SSH sessions is *less* great. You miss prompts. You forget which one is in which directory. The one that finished an hour ago is buried under three windows of vim. You alt-tab to the wrong terminal and start typing into the working session.

claws owns each `claude` process inside its own PTY and gives you one dashboard for all of them. Dots tell you which need you, which are working, which exited. Hit Enter, you're in. Ctrl-Space d, you're back. Sessions outlive your shell, your SSH connection, and your laptop's sleep cycle.

## Install

macOS:

```sh
brew install dodontommy/tap/claws-tui
```

Linux:

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/dodontommy/claws-tui/releases/latest/download/claws-tui-installer.sh | sh
```

Windows (PowerShell):

```powershell
powershell -c "irm https://github.com/dodontommy/claws-tui/releases/latest/download/claws-tui-installer.ps1 | iex"
```

The package is `claws-tui` (the unsuffixed `claws` slot on crates.io and Homebrew belongs to an unrelated AWS TUI), but the binary you run is just `claws`. `claude` (the Claude Code CLI) needs to be on your `PATH` — claws spawns it. Run `claws` to open the dashboard, `claws update` to upgrade in place.

## What you get

A small Rust daemon, a TUI client, and a clean separation: the daemon owns the processes, the client just renders.

- **Sidebar with status at a glance.** `●` working · `◐` idle · `★` needs you · `✗` exited. The top bar aggregates: `3● 1◐ 1★`.
- **Detail pane.** Last message, working directory, current branch (when in a git worktree), model, turn count, token usage, rough cost estimate, context window fill bar.
- **Grid view.** Press `g`. Same data, laid out as a wall of cards. Useful when you have eight sessions and need to scan them all at once.
- **Themes.** Press `t`. Catppuccin Macchiato, Tokyo Night, Nord, default, mono. Live preview as you arrow through; Esc reverts, Enter saves.
- **Worktree-aware spawning.** New session form lists every worktree of the current repo. Pick one with arrow keys; Enter spawns into it. Or hit `[+ new worktree]` and we run `git worktree add` for you with sane defaults.
- **Model picker on spawn.** Pill row: default · opus · sonnet · haiku. Pick one or just leave it on default.
- **`mkdir -p` on spawn.** Type a path that doesn't exist yet, hit Enter — we create it before launching Claude there.
- **SSH-friendly.** The daemon detaches from the controlling terminal at spawn. SSH disconnect doesn't kill it. Reconnect, run `claws` again, everything's still there.
- **Resume across reboots.** On daemon startup we re-spawn every session you didn't explicitly close, with `claude --resume <session-id>` so the conversation history is intact.
- **Bracketed paste.** Paste a multi-line snippet while attached — lands in Claude as one block instead of submitting on the first newline.
- **Auth-protected RPCs.** Other local user accounts can't drive your daemon to spawn `claude` as you. Token rotates per daemon startup; socket directory is mode `0700`.

## Day-to-day

Press `c` to spawn. The default cwd is your most-recently-active session's directory, so you usually just hit Enter to spawn another in the same workspace. Type a path; the next matching subdirectory shows as ghost text after your cursor — Right-arrow accepts it. Tab cycles `directory → worktrees → model → flags → directory`.

In the flags field, type whatever you want to pass to `claude` — `--system-prompt "be terse"`, `--effort xhigh`, `--add-dir <path>`, anything. `Ctrl-Y` toggles `--dangerously-skip-permissions` if you don't feel like typing it out (and you'll know which sessions are running with it: red `!` in the sidebar so you don't lose track).

Navigation:

- `j`/`k` or arrows — move through the sidebar
- `Enter` (or double-click) — attach to the selected session
- `/` — filter sessions by name, cwd, or title
- `i` — popup with the full last message + breakdown
- `g` — toggle sidebar/grid
- `t` — theme picker
- `?` — full keymap

Session management:

- `r` — rename. Renamed names override Claude's auto-generated titles.
- `R` — kill and immediately `claude --resume`. Useful when Claude gets stuck mid-tool.
- `x` — close and forget. Same key forgets `✗ resume failed` rows.
- `q` — exit the TUI (the daemon stays running)

## Attached mode

While attached, `Ctrl-Space` is the prefix key. Same idea as tmux's `Ctrl-b`, just doesn't collide with shell line-edit:

- `Ctrl-Space d` — detach back to the dashboard
- `Ctrl-Space n` / `p` — cycle to the next/previous session without going through the dashboard
- `Ctrl-Space 1`..`9` — jump directly to session N
- `Ctrl-Space [` — enter scroll mode to read back through history

Paste a multi-line snippet and it stays as one block. Forward-pass to Claude, no premature submit.

## SSH and persistence

This part works the way you'd want. SSH in, run `claws`, spawn a few, do some work, disconnect. Sessions keep running on the remote. Come back tomorrow, SSH back in, `claws` again, everything's still there. The dashboard remembers what was active most recently, so the cursor lands somewhere useful.

The daemon's Unix socket lives at `/tmp/claws-<uid>/sock`. That path sticks around across logouts (intentionally — `XDG_RUNTIME_DIR` gets wiped by systemd-logind when your last session closes, which would orphan a still-running daemon).

After a reboot the daemon is gone but the JSONL transcripts Claude writes aren't. On next startup the daemon resumes everything you hadn't explicitly closed.

## Configuration

Pretty much none, by design. Default colors, keymap baked in, sensible behavior on a fresh install. Theme is the one persistent setting — picked once, remembered.

Per-session settings (cwd, flags, name, model) survive across resumes via a small SQLite file in your local data directory. You don't manage it; it manages itself.

If your custom Claude `statusLine` doesn't expose context-window fill in `<n>% used/total` format, claws falls back to estimating it from token counts and a built-in per-model context limit. Approximate, close enough.

## Stopping the daemon

`q` exits the TUI but leaves the daemon and your sessions running. To actually stop everything:

```sh
claws kill-server
```

If the daemon is wedged (used to happen on rare auth-token races; v0.2.6 nailed the simultaneous-start case and v0.2.9 closed the late-start case), there's a force escape hatch:

```sh
claws kill-server --force
```

That PID-kills the daemon process directly, bypassing the auth-protected shutdown RPC.

## Architecture, briefly

```
  ┌─────────────┐         ┌────────────┐
  │  TUI client │ ──RPC──▶│  daemon    │──┬─ PTY ─▶ claude --session-id ...
  └─────────────┘  unix   │            │  ├─ PTY ─▶ claude --resume ...
        ▲         socket  │  registry  │  └─ PTY ─▶ claude --resume ...
        │                 │  + state   │
        │                 │  + auth    │
        └─── attached ────┘
              streaming
```

The daemon is a long-lived process that owns the PTYs, parses each session's vt100 stream into an internal screen, persists session metadata to SQLite, and exposes a JSON-RPC API over a Unix socket. The TUI is a regular `claws` invocation that connects, lists sessions, and (when you attach) streams PTY bytes through.

The RPC channel is symmetric: hooks emitted by Claude itself flow back through `claws hook-emit` into the same daemon, which is how the dashboard knows when a session enters `awaiting_permission` or finishes a tool call.

## Built with

Rust. `ratatui` for the UI, `portable-pty` for cross-platform PTY spawning, `vt100` for terminal emulation, `interprocess` for the local socket, `rusqlite` for state, `axoupdater` for in-place updates.

## Phone companion

A real phone app, as of v0.3.0. PWA (no App Store, no install pipeline) that pairs to your daemon over Tailscale or any HTTPS fronting service. Once paired, you have:

- Live status of every session in your daemon
- Push notifications when one enters `awaiting_permission` or exits — pings your phone even when the app is closed
- One-tap Allow / Always-allow / Deny when a permission prompt fires
- Full xterm rendering of Claude's TUI, sized to your phone (per-client virtual screens, same model tmux uses)
- Real keyboard typing into a live session
- Spawn-from-phone with cwd, model picker, flags, and optional `mkdir -p`
- Theme picker matching the TUI's themes (default, catppuccin, tokyo night, nord, mono)

To set up:

```sh
# on your dev box, once
tailscale serve --bg --https=443 http://127.0.0.1:9817
claws phone start
claws phone pair --url https://<your-machine>.<your-tailnet>.ts.net
```

Scan the QR on your phone, install to home screen, allow notifications. Future `claws phone pair` runs reuse the saved URL — no flag needed.

## What it doesn't do

- It doesn't multiplex on the same `claude` process — each session is its own subprocess. Use Claude's own session-resume model for continuity within one conversation, and claws to manage *between* conversations.
- It doesn't do shared multi-user access. The auth token is per-user; the socket is mode 0700. If you want a team-shared dashboard, that's a different tool.
- The phone PWA is iOS-Safari-tested; Android Chrome should work in principle (Web Push, service worker, visualViewport are all standard) but isn't verified.

## Security

The daemon rejects any RPC without the per-startup auth token. The socket directory is mode `0700` on Unix; on Windows it lives under `%LOCALAPPDATA%\claws\` whose default ACL restricts to the current user. Sessions running with `--dangerously-skip-permissions` get a red `!` in the sidebar to keep them visually distinct from gated ones. Threat model is documented in [SECURITY.md](SECURITY.md).

## License

MIT or Apache-2.0, whichever fits your project.
