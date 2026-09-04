# `al` — agent loader

[![Release CI](https://github.com/0x8f701/agent-loader/actions/workflows/release.yml/badge.svg)](https://github.com/0x8f701/agent-loader/actions/workflows/release.yml)
[![License](https://img.shields.io/badge/license-MIT-blue)](./LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.85%2B-orange?logo=rust)](./Cargo.toml)
[![Platform](https://img.shields.io/badge/platform-macOS%20%C2%B7%20Linux%20%C2%B7%20Windows-lightgrey)](./install.sh)

`al` is a session catalog and launcher for AI coding assistants. It discovers session exports on disk, lists and searches them locally, converts a session from one tool's format to another, and can fork or reopen sessions in the target agent.

[Installation](#installation) ·
[Usage examples](#usage-examples) ·
[Commands](#commands) ·
[Supported tools and conversion behavior](#supported-tools-and-conversion-behavior) ·
[Building from source](#building-from-source) ·
[Releasing](#releasing) ·
[License](#license)

## Installation

> **Note:** the install scripts download prebuilt binaries from [GitHub Releases](https://github.com/0x8f701/agent-loader/releases). A published release is required; until one exists, build `al` from source ([Building from source](#building-from-source)).

Published releases contain prebuilt single-file binaries for macOS (arm64/x86_64), Linux (arm64/x86_64, glibc 2.31 or newer), and Windows (x86_64).

```sh
# macOS / Linux
curl -fsSL https://raw.githubusercontent.com/0x8f701/agent-loader/main/install.sh | bash
```

```powershell
# Windows PowerShell
irm https://raw.githubusercontent.com/0x8f701/agent-loader/main/install.ps1 | iex
```

The installer verifies every download against the release's `SHA256SUMS` and installs the binary under `~/.agent-loader/bin/al` (`%USERPROFILE%\.agent-loader\bin\al.exe` on Windows). On Unix it adds the install directory to your shell profile when needed; on Windows it updates the user-environment `PATH`.

Pin a specific release:

```sh
curl -fsSL https://raw.githubusercontent.com/0x8f701/agent-loader/main/install.sh | bash -s -- --version v0.5.1"
```

If `al` is not on PATH after installation, open a new terminal or use the full path printed during install.

## Usage examples

```sh
al --version
al --help

# Local listing (default 5 rows, most recent first).
al sessions
al sessions 20
al sessions --all

# Local message-body search.
al sessions search "refactor auth"
al sessions search --dedupe --picker "database migration"

# Read-only multi-host listing. Hosts are queried in order; output is grouped
# per host. Requires `al` to be installed and on PATH on each remote host.
al sessions --host host-a --host host-b --host local
al sessions list --all --dedupe --host host-a --host host-b

# Convert a Pi export into a Claude-compatible export.
al sessions convert pi claude /workspace/project/pi-session.jsonl /workspace/project/claude-session.jsonl

# Relocate sessions from one directory to another (catalog folder or recorded cwd).
al sessions move /workspace/old-project /workspace/new-project
al sessions move ~/.pi/agent/sessions/--old-project-- ~/.pi/agent/sessions/--new-project--

# Convert a Grok session for OMP (the `migrate` spelling is a visible alias).
al sessions migrate grok omp /workspace/project/grok-session.jsonl /workspace/project/omp-session.jsonl

# Convert any supported source into a Hyper payload.
al sessions convert claude hyper /workspace/project/claude-session.jsonl /workspace/project/hyper-session/

# Print the command without launching the agent. A cross-tool fork may first write the forked export.
al sessions open 00000000-0000-0000-0000-000000000000 claude --print-command
al sessions fork 00000000-0000-0000-0000-000000000000 grok --print-command
al sessions open agent-session-id agent --print-command

# Interactive fuzzy session picker (local only; requires fzf on PATH).
al sessions --fzf
al sessions query "refactor auth"

# Point-to-point session catalog sync (dry-run first).
# Cursor Agent native SQLite stores are intentionally not synchronized.
al sessions sync host-a --dry-run
al sessions sync host-a host-b --tool omp --tool pi --dry-run

# Run one shell-compatible command string in a tmux-managed pane (Unix only).
al tmux-run -c /workspace/project --fresh -- 'make test'

# Launch Cursor's official `agent` CLI. Bare `agentlo` tries to continue
# Cursor's latest chat, then starts a new chat if `--continue` exits nonzero.
al agentlo
al agentlo chat-123 "fix the parser"
al agentlo --session chat-123 "fix the parser"
al agentlo --tmux
al agentlo --worktree feature-name
al agentlo --host host-a --wt feature-name --tmux
```

## Commands

Run `al --help` and `al COMMAND --help` for the current argument surface.

- `al sessions [COUNT]` — list recent local sessions (default 5; use `--all` to show everything, `--dedupe` to keep the newest row per tool/cwd/summary).
  - `al sessions list [COUNT]` — explicit list with the same flags.
  - Repeatable `--host HOST` adds a remote host to query. `local` is reserved for the current machine. With `--host`, `al` runs `al sessions list` on each host over SSH and prints read-only results grouped under `== <host> ==`. It is not a sync: no files are copied. Hosts are queried in the order given; `--all`/`--dedupe`/COUNT are forwarded to each remote. Per-host deduplication only; there is no cross-host deduplication. The command continues after a failed host and exits nonzero if any host failed. Empty, whitespace-containing, control-character, or option-like host values are rejected. `--host` cannot be combined with `--paths`, `--picker`, or `--fzf`.
  - `al sessions --fzf` / `al sessions list --fzf` — local interactive fuzzy filter over tool, time, session id, and summary, followed by a target picker that opens the session. Native Agent rows offer only `agent` and default to it. Requires `fzf` on PATH. Uses the full deduped catalog, not the default 5-row list.
  - `al sessions search QUERY` — search the text of user/assistant messages in discovered sessions. Search is local-only. `--dedupe` and `--picker` change output style.
  - `al sessions query QUERY...` — the same picker-and-open flow as `--fzf`, after a local user/assistant message-body search, including parsed native Agent messages. Requires `fzf` on PATH.
  - `al sessions convert SOURCE TARGET INPUT [OUTPUT]` (visible alias `migrate`) — read `INPUT` in the native format of `SOURCE` and write a `TARGET`-compatible export. If `OUTPUT` is omitted, the export is written to the target tool's native session location and the path is printed. Cursor Agent is intentionally excluded because its store format is undocumented.
  - `al sessions move FROM TO [--tool TOOL]... [--dry-run]` — move native session files from one directory to another without converting them. `FROM` can be a catalog folder (match by file path) or a recorded workspace path (match by `cwd`, including after the project directory itself is gone). `TO` can be another catalog folder, a dump directory, or the new workspace: matching `cwd` values are rewritten and files are re-homed to that tool's native layout. Cursor Agent stores are excluded. The command refuses to overwrite an existing destination, to move an entire home/catalog root, or to delete a Grok directory that contains unexpected files.
  - `al sessions fork SESSION_REF TARGET` — fork a session to another tool. Agent is not a fork target and native Agent sessions cannot be forked.
  - `al sessions open SESSION_REF TARGET` — reopen a session in the target tool. A native Agent session may only target `agent`; it runs `agent --force --trust --approve-mcps --resume <session-id>` in the recorded cwd.
  - `al sessions open|fork --print-command` — print the native command without launching the agent, then exit. A cross-tool fork may first write the forked export.
  - `al sessions sync SRC_OR_DST [DST] [--tool TOOL]... [--dry-run]` — synchronize supported session catalogs point-to-point. With one endpoint, the local catalog is uploaded to that endpoint. With two endpoints, the first is the source and the second is the destination; both cannot be `local`. `--tool` can be repeated to limit the transfer to specific source tools. Cursor Agent SQLite stores are excluded and `--tool agent` is rejected. This is separate from read-only multi-host listing.
- `al omlo|pilo|rpilo|grolo|hyperlo|dolo|colo|cclo|agentlo [...]` — launch the corresponding coding agent (OMP, Pi, Rpi, Grok, Hyper, Droid, Codex, Claude, Cursor Agent). Common launcher flags:
  - `--host HOST` — run on a remote host over SSH (requires the current directory to be inside a git repository).
  - `--wt NAME` — use a named git worktree on the remote host (requires `--host`).
  - `--tmux` — wrap the launch in tmux (Unix only).
  - `--session=ID` — pass a session selector to the underlying tool.
  - `--` — protects everything after the delimiter from being interpreted as launcher flags, forwarding it verbatim to the agent.

  On macOS, remote launchers map `/Users/<user>` to `/home/<user>`. Additional component-aware mappings can be supplied through `AL_REMOTE_PATH_MAPS` as an ordered JSON array of absolute source/destination pairs, for example `[["/Volumes/workspace","/srv/workspace"]]`. Every source and destination must be an absolute path; malformed configuration fails before SSH is invoked. The remote host must have `al` installed and on PATH.

  `al agentlo` launches Cursor's official `agent` CLI. With no tool args it first runs `agent --force --trust --approve-mcps --continue`; if that command exits nonzero, it retries as `agent --force --trust --approve-mcps` to create a new chat. The continue probe hides Cursor's "No previous chats found." status line, including inside `--tmux`. Cursor's native local worktree options (`-w`/`--worktree [NAME]` and `--worktree-base REF`) pass through normally. The launcher-level `--wt NAME` remains the remote-host worktree control and therefore requires `--host`; `--tmux` works for both local and remote launches. A `--session ID` selector (or a positional chat id) maps to `--resume ID`; any other arguments are forwarded verbatim after the base approval flags. Separately, `al sessions`, `al sessions search`, `al sessions --fzf`, and `al sessions query` discover native Cursor Agent sessions and can reopen them exactly; conversion and sync remain disabled because the native SQLite/blob format is undocumented and live stores may depend on WAL state.
- `al tmux-run ...` — run a command inside the tmux integration wrapper (Unix only; Windows returns an explicit unsupported-platform error).

```sh
# One command argument is evaluated by the login shell for script compatibility.
al tmux-run -c /workspace/project --fresh -- 'make test'

# Two or more command arguments preserve exact argv (no shell).
al tmux-run -c /workspace/project --fresh -- python -m pytest tests/

# Force exact argv even with one executable argument.
al tmux-run --argv -c /workspace/project -- /bin/cat
```

Common `tmux-run` flags: `--no-attach`, `--fresh`, `-s session`, `-n window`, `-c cwd`, `-L socket-name | -S socket-path`, and `--`.

## Supported tools and conversion behavior

`al` recognizes the following tools:

| Role | Tools |
|------|-------|
| Source (discover/search) | `pi`, `rpi`, `omp`, `droid`, `codex`, `claude`, `grok`, `agent` |
| Target (convert to / launch) | `pi`, `rpi`, `omp`, `droid`, `codex`, `claude`, `grok`, `hyper`; `agent` is open-only for native Agent sessions |

`grok` and `hyper` share the same storage layout; a Grok session can be converted to Hyper in place, and Hyper targets reuse Grok's native format. Rpi is a separate catalog at `~/.rpi/sessions`; the on-disk JSONL is Pi-compatible, so `al` can open a Pi session with `rpi` (and the reverse) without converting.

Cursor Agent discovery reads `.cursor/chats/<32-hex-workspace-hash>/<session-id>/store.db` exactly two directories beneath the chats root, plus the sibling `meta.json` sidecar for cwd, title, and timestamps. The SQLite database is opened read-only. Cursor's store format is undocumented and `al` never emits or converts sessions into it; malformed individual stores are skipped without hiding other catalog rows.

The managed `rpi` executable is resolved from `$PI_HOME/bin/rpi`, then
`~/.rpi/bin/rpi`, then `PATH`. Rpi sessions are discovered under `~/.rpi/sessions`.

Session conversion is intentionally lossy:

- Only user/assistant messages and the first recognized text block from each message are preserved. Images, attachments, tool calls, and other metadata are dropped.
- Empty lines are skipped. After a successful native load, unparseable records and non-message entries may be skipped.
- Generated summaries normalize whitespace and are truncated to 100 characters; projected message text is preserved.
- `grok` and `hyper` targets reuse Grok's storage layout; `pi` writes `~/.pi/agent/sessions` and `rpi` writes `~/.rpi/sessions` using the same JSONL shape; other targets write their own native formats.

This is by design: `al` is a loader that passes the useful context forward, not a bit-perfect archival mirror.

## Source format note

Format adapters read each tool's native export:

- **Pi / OMP** — newline-delimited JSONL conversation trees.
- **Droid** — `session_start` and `message` typed records.
- **Codex / Claude / Grok** — tool-specific JSON/JSONL or directory layouts. Hyper targets reuse Grok's storage layout. Rpi uses Pi's JSONL shape under `~/.rpi/sessions`.
- **Cursor Agent** — a read-only SQLite `store.db` adapter with sibling `meta.json`; only plaintext user/assistant text is projected, injected wrappers and tool output are excluded, and the native store remains canonical.

Pi/OMP nonempty files require a valid native `session` header or loading fails. After a successful load, later unparseable or non-message records may be skipped.

## Building from source

You need a recent stable Rust toolchain. The MSRV is Rust 1.85.

```sh
git clone https://github.com/0x8f701/agent-loader
cd agent-loader
cargo run              # build and run locally
cargo build --profile release-dist
./target/release-dist/al --version
```

The `release-dist` profile strips symbols and enables thin LTO for a small, single-file binary.

## Releasing

1. Update `Cargo.toml` `[package] version` to the release version.
2. Commit on `main`.
3. Tag and push. The tag must match the `Cargo.toml` version exactly:

```sh
VERSION=$(grep -m1 '^version' Cargo.toml | sed -E 's/^version *= *"([^"]+)".*/\1/')
git tag "v${VERSION}"
git push origin main "v${VERSION}"
```

CI builds the five targets below, packages each archive, generates `SHA256SUMS`, and publishes a GitHub Release.

Workflow: [`.github/workflows/release.yml`](.github/workflows/release.yml)

### Artifacts

| Asset | Example |
| macOS arm64 | `al-0.5.1-aarch64-apple-darwin.tar.gz` |
| macOS x86_64 | `al-0.5.1-x86_64-apple-darwin.tar.gz` |
| Linux x86_64 (glibc 2.31+) | `al-0.5.1-x86_64-unknown-linux-gnu.tar.gz` |
| Linux arm64 (glibc 2.31+) | `al-0.5.1-aarch64-unknown-linux-gnu.tar.gz` |
| Windows x86_64 | `al-0.5.1-x86_64-pc-windows-msvc.zip` |
| Checksums | `SHA256SUMS` |

The tag must match `Cargo.toml` version exactly (`v0.5.1` ↔ `0.5.1`) or the build fails.

## License

MIT. See [`LICENSE`](./LICENSE).
