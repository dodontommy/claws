# Changelog

All notable changes to claws are listed here. Format loosely follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), versions follow
[SemVer](https://semver.org/).

## [0.3.0] — 2026-05-03

### Added — phone companion (PWA)

A real phone app for claws. Pair via QR over Tailscale (or Cloudflared, or
any HTTPS fronting service), open the URL on your phone, install to home
screen. The PWA gives you live status of every session in your daemon,
push notifications when one needs you, a one-tap allow/deny for permission
prompts, full xterm-rendered terminal output, real keyboard typing into
attached sessions, and a spawn form mirroring the TUI's `c` modal.

- **Live session list**, status counts, theme picker matching the TUI
  (default, catppuccin mocha, tokyo night, nord, monochrome). Persisted
  per-device via localStorage.
- **xterm.js terminal** rendering Claude's TUI at the phone's geometry
  via per-client virtual screens — each phone connection holds its own
  vt100 parser fed from the same byte stream, plus a real PTY resize so
  Claude itself draws for the phone (tmux's multi-client model).
- **iOS keyboard input** that actually works. Uses Ace editor's
  textinput pattern (Cloud9 / Replit mobile): a 1×1 fixed-position
  textarea whose value cycles between a placeholder and
  placeholder+keystrokes, never empty. Sidesteps every iOS Safari
  keyboard-dismissal trigger we hit (and we hit them all).
- **Web Push** with VAPID keys generated lazily on first start. The
  daemon fans out a notification when any session enters
  `awaiting_permission` or exits — phone pings even when the app is
  closed. Tap to deep-link straight to that session.
- **Permission prompt banner** with `1 / 2 / 3` quick-action buttons
  for the common Allow / Always-allow / Deny choice.
- **Spawn-from-phone bottom-sheet** mirroring the TUI's spawn modal:
  cwd, model picker (default · opus · sonnet · haiku), flags. Optional
  `mkdir -p` if the directory doesn't exist yet.
- **Tailscale auto-detection** for the pair-code QR. Run
  `tailscale serve --bg --https=443 http://127.0.0.1:9817`, then
  `claws phone pair` finds the matching tailnet hostname and encodes
  the right URL into the QR. Persisted with `--url`.
- **`--pwa-dir`** for hot-reloading PWA assets without `kill-server`.
  Edit a JS/HTML/CSS file in `phone-pwa/dist/`, refresh the phone, see
  changes — daemon stays up, sessions undisturbed.

### Added — TUI

- Bracketed paste mode is now enabled (v0.2.11). Multi-line paste while
  attached lands in Claude as one block instead of submitting on the
  first newline.

### CLI

- `claws phone start [--bind ADDR] [--pwa-dir PATH]`
- `claws phone pair [--url HTTPS_URL]` — prints code + QR, persists URL
- `claws phone status | stop | devices | revoke <id>`

### Known limitations

- The phone PWA's typing uses a real `<input>` overlay (Ace's iOS
  textinput pattern) — characters echo back through xterm because
  Claude responds in the PTY, not because the input is local-echo.
- Multi-client cost: while the phone is attached, the dev-TUI viewing
  the same session sees Claude redraw at the phone's geometry. Detach
  the phone (or resize the dev terminal) and the dev TUI's preferred
  size returns. Same tradeoff as `tmux attach` from a smaller window.
- iOS Safari only — no Android Chrome testing yet, though it should
  work in principle (Web Push, service worker, visualViewport are all
  standard).

## [0.2.11] — 2026-05-03

### Fixed
- Multi-line paste while attached to a session no longer submits the
  first line and drops the rest. Without bracketed-paste mode, crossterm
  was decomposing pasted text into a stream of individual key events; the
  first newline became a Press(Enter) that Claude saw and submitted on.
  We now enable bracketed paste at TUI init, listen for Event::Paste, and
  forward the payload wrapped in `\x1b[200~ ... \x1b[201~` so Claude's
  own bracketed-paste handling treats the whole thing as one block.

### Internal
- Cleared the five stale `cargo build` warnings: gated the Windows-only
  `Command` import in pidfile.rs, dropped never-read fields/methods from
  persist.rs / ring.rs, removed unused protocol scaffolding. Zero
  warnings on a fresh build.

## [0.2.10] — 2026-05-03

### Added
- Spawn form now has a model picker — pill row between worktrees and flags
  with `default · opus · sonnet · haiku`. Left/Right arrows cycle when the
  row is focused; the chosen value is passed to `claude --model`. `default`
  means don't pass the flag and let claude pick.
- Spawn form mkdir-on-submit: typing a directory that doesn't exist now
  shows an italic `[enter to mkdir -p]` hint inline with the path. Enter
  runs `mkdir -p` for you and proceeds with the spawn instead of refusing.
- Tab order updated to follow the new visual top-to-bottom flow: directory
  → worktrees (if inside a repo) → model → flags → directory.

## [0.2.9] — 2026-05-03

### Fixed
- Two daemons could end up running on the same socket. The startup path
  used to `remove_file(&sock)` unconditionally before binding, so a second
  daemon spawned later would clobber the first's filesystem entry, bind a
  fresh inode at the same path, and run `auto_resume` — which then spawned
  duplicate `claude --resume <id>` children for every persisted session,
  bricking active conversations as the two claudes fought over the same
  JSONL transcript. New protocol on startup: try the bind first; on
  failure, probe the socket with a short-timeout connect; if a daemon is
  listening, log and exit cleanly without touching anything; only remove
  the socket file (and retry the bind) when the connect fails, which
  identifies a stale path from a crashed daemon. The v0.2.6 fix covered
  the simultaneous-start race; this covers the late-start race that was
  causing recurring duplicate-daemon reports.

## [0.2.8] — 2026-05-03

### Fixed
- Sidebar rows for two concurrently-working sessions used to swap places
  every tick because the sort tiebreaker was `last_activity_ms`, which
  both rows update constantly while streaming. Active buckets
  (awaiting-permission, streaming, spawning) now sort by spawn order
  (session id), so live sessions hold a stable position. Quiet buckets
  (idle, exited, resume-failed) keep the recency tiebreaker since
  "what did I touch last" is the useful signal there.

## [0.2.7] — 2026-05-02

### Added
- `claws kill-server --force` PID-kills the daemon, bypassing the
  auth-protected shutdown RPC. The escape hatch for any future
  state-file weirdness that leaves the daemon unable to authenticate
  its own kill request. Daemon now writes `state_dir/daemon.pid` at
  startup and removes it on graceful shutdown. Only the user who
  owns the daemon can read the PID file (state_dir permissions),
  so this doesn't widen the attack surface.

## [0.2.6] — 2026-05-02

### Fixed
- Daemon-startup race that left the auth token on disk out of sync
  with the running daemon's in-memory token. Two near-simultaneous
  daemon-starts (e.g. TUI auto-spawn + a parallel CLI invocation, or
  the TUI relaunched too quickly) had the second daemon overwrite
  the first's `auth.token` before failing to bind, after which every
  client request was rejected by the running daemon as `-32001
  unauthorized`. Fix: bind the socket first; the failing daemon now
  exits without ever touching the token file.

## [0.2.5] — 2026-05-02

### Changed
- Dropped mouse capture entirely. Capturing mouse events disabled the
  terminal's native drag-to-select / copy-paste, which is the more
  valuable interaction. We lose double-click-attach and scroll-wheel
  navigation in the sidebar — both already had keyboard equivalents
  (`Enter` and `j`/`k`).

## [0.2.4] — 2026-05-02

### Fixed
- Sessions could get stuck on `spawning` status forever if Claude's
  `SessionStart` hook didn't fire (custom builds, hook config not
  picked up, slow first frame). PTY bytes flowing into the ring
  buffer now also promote out of `spawning` as a fallback.
- "Needs you" was falsely flagged on plain idle sessions. Claude's
  `Notification` hook fires for many non-permission things (info
  messages, completion notices) and we were treating all of them as
  "awaiting permission". Only `PermissionRequest` is the real signal
  now; sessions stuck mid-`Notification` no longer leak the awaiting
  state forever.

### Changed
- The persistent row-wide bg tint on awaiting-permission rows is gone;
  it competed with the selection gutter and made awaiting rows look
  like the selected row at a glance. The 500ms transition flash, the
  pulsing glyph (★/✦), and the "needs you" label still mark the state.

### Added
- Screenshot in the README (`docs/screenshot.png`) — populated dashboard
  with mixed states, worktree branch info, and a flagged dangerous
  session, on the catppuccin theme.

## [0.2.3] — 2026-05-02

### Reverted
- Reverted the pulsing `!` indicator from v0.2.1/v0.2.2. The static red
  bold `!` from v0.2.0 is back. The pulse turned out subtle enough that
  it didn't earn its complexity, and "border pulse" is a different and
  bigger UX call we're not making yet.

## [0.2.2] — 2026-05-02

### Fixed
- The pulse on the `--dangerously-skip-permissions` `!` indicator was
  invisible on the tokyo-night and mono themes because both define
  `context_high == awaiting_a` (i.e. the v0.2.1 cycle was between two
  identical colors). Now cycles between `context_high` and
  `awaiting_b` (always distinct in every shipped theme) and toggles
  `BOLD` as a belt-and-suspenders fallback.

## [0.2.1] — 2026-05-02

### UX
- The red `!` indicator on sessions running with
  `--dangerously-skip-permissions` now pulses (fg cycles between
  `theme.context_high` and `theme.awaiting_a` every 3 ticks). Slower
  cadence than the awaiting-permission pulse so the two flavors of
  attention read as distinct things at a glance. Applies to the
  sidebar, grid card title, and attached header.

## [0.2.0] — 2026-05-02

### Worktrees
- **Spawn form is now worktree-aware.** When the typed cwd is inside a
  git repo, claws lists every worktree of that repo (parsed from
  `git worktree list --porcelain`). Tab into the section, pick one to
  populate cwd, or hit `[+ new worktree]` to open a sub-form for branch
  name + path. Defaults: branch is `<repo>-2`/`-3`/... (deduped against
  existing branches), path is `<parent>/<repo>-<branch>` (deduped
  against existing paths and disk).
- If the cwd already has a live session, a `⚠ session already running
  in this directory` banner appears between recents and worktrees. We
  warn but don't block — claws shouldn't refuse what you explicitly
  asked for.
- Worktrees are never auto-removed. They're independent git artifacts
  with their own lifecycle; closing the session leaves the worktree
  intact for re-attachment later.
- Enter on an existing worktree row spawns the session in that worktree
  in one keystroke — no second-Enter to confirm.
- New-worktree form notes that git auth (gh, ssh keys) needs to be set
  up if claude will push or fetch inside the worktree.
- Branch name surfaced as a `branch` row in both the dashboard detail
  pane and the Details modal. Cached per session; cleared on F5.

### Spawn form
- Rewrote layout: 3-column left padding, blank rows between sections,
  shorter labels, footer in the kbd-pill style matching the dashboard.
- Default cwd is now the most-recently-active session's cwd (falls back
  to current_dir if no sessions exist).
- Ghost-text path autocomplete: as you type, the lexicographically first
  matching subdir appears as dim italic suffix. Right-arrow at end of
  line accepts. Case-sensitive on Unix, case-insensitive on Windows.
- Skip-permissions hint moved beneath the flags input (was wedged
  between cwd and flags).
- Tab order matches the visual top-to-bottom layout:
  cwd → worktrees → flags → cwd.

### Security
- **Per-daemon-startup auth token gates every RPC call.** Closes a gap on
  Windows where the default named-pipe ACL would have allowed any local
  user account to connect to your daemon and ask it to spawn `claude`
  with arbitrary flags as you. The token lives at `state_dir/auth.token`
  (mode 0600 on Unix; default `%LOCALAPPDATA%` ACL on Windows). Old
  clients without the token field are rejected with `unauthorized`.
- Documented threat model in `SECURITY.md`.

### UX
- Theme picker modal on `t` (live preview, esc reverts, enter saves).
- Grid view toggle on `g`. Multi-column wall of session cards.
- Top bar shows aggregate state counts (`3● 1◐ 1★ ✗`).
- Footer reflects current state: theme name and `g <next-mode>`.
- Detail pane has a 3-block stats strip (`tokens │ context │ cost`),
  promoted context bar, quoted-block treatment for the last message.
- Sidebar entries: louder "needs you" pulse, glyph cycles ★/✦, row-wide
  bg tint, cwd basename right-aligned. State-transition flash for ~500ms
  when status changes.
- Help modal is scrollable with ↑/↓/j/k/PageUp/PageDown.
- Resume failures surface as `✗ resume failed` rows in the dashboard
  instead of vanishing silently. Press `x` to forget them.
- Sessions running with `--dangerously-skip-permissions` get a red `!`
  indicator in the sidebar, card title, and attached header.
- Mouse double-click attaches to a session (the `handle_mouse` path was
  defined but never wired to the run loop until now).
- `claws_version` field on every RPC; daemon logs version skew.

### Themes
- All surfaces (detail pane, attached header/footer, modals, cards)
  consistently use `theme.*` instead of hardcoded colors. The 5-theme
  story now actually applies everywhere.

### Tests / CI
- Unit tests for protocol round-trip + legacy-format compat, paths shape,
  ring buffer, persist store, hook settings + Windows path normalization,
  auth token I/O + rotation, git porcelain parser + path/branch suggesters.
  27 tests total.
- New `.github/workflows/ci.yml` runs `cargo build` + `cargo test` on
  Linux/macOS/Windows on push and PR. fmt + clippy run advisory.

### Fixed
- Mouse events were being dropped by the run loop. Now wired to
  `handle_mouse`, enabling double-click attach.
- Detail pane's `▎` quote bar painted full-height of the message section
  even for short messages. Now matches actual content rows.

### Security
- **Per-daemon-startup auth token gates every RPC call.** Closes a gap on
  Windows where the default named-pipe ACL would have allowed any local
  user account to connect to your daemon and ask it to spawn `claude`
  with arbitrary flags as you. The token lives at `state_dir/auth.token`
  (mode 0600 on Unix; default `%LOCALAPPDATA%` ACL on Windows). Old
  clients without the token field are rejected with `unauthorized`.
- Documented threat model in `SECURITY.md`.

### Added
- Theme picker modal on `t` (live preview, esc reverts, enter saves).
- Grid view toggle on `g`. Multi-column wall of session cards.
- Top bar shows aggregate state counts (`3● 1◐ 1★ ✗`).
- Footer reflects current state: theme name and `g <next-mode>`.
- Detail pane has a 3-block stats strip (`tokens │ context │ cost`),
  promoted context bar, quoted-block treatment for the last message.
- Sidebar entries: louder "needs you" pulse, glyph cycles ★/✦, row-wide
  bg tint, cwd basename right-aligned. State-transition flash for ~500ms
  when status changes.
- Help modal is scrollable with ↑/↓/j/k/PageUp/PageDown.
- Spawn form: `[ctrl-y] skip permissions: on/off` indicator above the
  flags field; recent dirs render as `basename  ~/dim/path`.
- Resume failures surface as `✗ resume failed` rows in the dashboard
  instead of vanishing silently. Press `x` to forget them.
- Mouse double-click attaches to a session.
- `claws_version` field on every RPC; daemon logs version skew.
- Unit tests for protocol round-trip, paths, ring buffer, persist store,
  hook settings, and auth token I/O. CI workflow runs `cargo build` +
  `cargo test` on Linux/macOS/Windows.

### Changed
- Detail pane and modals use the active theme throughout (previously the
  detail pane and several modals hardcoded default-theme colors).
- Help modal: keys rendered as kbd-style pills, section headers get a
  `▎` left bar.
- Theme switching no longer cycles silently — it opens the picker.

### Fixed
- Mouse events were being dropped by the run loop. Now wired to
  `handle_mouse`, enabling the existing double-click attach.

## [0.1.6]

- Empty-state ASCII crab and figlet wordmark.
- Theme system with five built-in themes (default, catppuccin mocha,
  tokyo night, nord, monochrome).
- Renamed CLI from `multi-claude` to `claws`.

## [0.1.0]

- Initial public release.
