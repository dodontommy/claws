# Security policy

## Reporting a vulnerability

If you find a security issue in claws, please **do not open a public GitHub
issue**. Instead, email the maintainer:

- **machine.valley@gmail.com**

Or use GitHub's private vulnerability reporting:

- https://github.com/dodontommy/claws/security/advisories/new

I'll acknowledge receipt within a few days. Please include:

- A description of the issue and the impact you think it has.
- Steps to reproduce, ideally with a minimal example.
- The version of claws you're running (`claws --version`) and your OS.

## Supported versions

Only the most recent release line gets security fixes. If you're on an
older version, the upgrade path is `claws update` (for installer-based
installs) or reinstalling via the official installer scripts in the README.

| Version | Supported |
| ------- | --------- |
| 0.1.x   | ✅        |
| < 0.1.0 | ❌        |

## Threat model

claws is a single-user tool: the daemon runs as the user who started it
and the TUI/CLI talk to that daemon over a local IPC socket. The intended
trust boundary is "everything running as this user is trusted; other local
users and remote attackers are not."

Specifically:

- **Other local user accounts on the same machine** should not be able to
  drive your daemon. Each daemon writes a per-startup token to its
  state directory (which on every supported OS is restricted to the
  owning user by default permissions). Every RPC request carries that
  token, and the daemon rejects mismatches. This closes the gap that
  Windows named pipes — which by default allow any local user to
  connect — would otherwise leave open.

- **The contents of `claude` sessions are out of scope**. claws doesn't
  read or modify Claude Code's JSONL transcripts; if those leak, that's
  a Claude Code question, not a claws one.

- **Remote attackers** are out of scope unless they already have a
  shell as your user (in which case they have everything anyway).

If your threat model differs (e.g. shared CI runner, sudoer escalation
testing), I'd love to hear about it.
