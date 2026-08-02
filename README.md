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
curl -fsSL https://raw.githubusercontent.com/0x8f701/agent-loader/main/install.sh | bash -s -- --version v0.2.0
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

# Convert a Grok session for OMP (the `migrate` spelling is a visible alias).
al sessions migrate grok omp /workspace/project/grok-session.jsonl /workspace/project/omp-session.jsonl

# Convert any supported source into a Hyper payload.
al sessions convert claude hyper /workspace/project/claude-session.jsonl /workspace/project/hyper-session/

# Print the command without launching the agent. A cross-tool fork may first write the forked export.
al sessions open 00000000-0000-0000-0000-000000000000 claude --print-command
al sessions fork 00000000-0000-0000-0000-000000000000 grok --print-command

# Interactive fuzzy session picker (local only; requires fzf on PATH).
al sks
al skss "refactor auth"

# Point-to-point session catalog sync (dry-run first).
al sessions sync host-a --dry-run
al sessions sync host-a host-b --tool omp --tool pi --dry-run

# Run one shell-compatible command string in a tmux-managed pane (Unix only).
al tmux-run -c /workspace/project --fresh -- 'make test'
```

## Commands

Run `al --help` and `al COMMAND --help` for the current argument surface.

- `al sessions [COUNT]` — list recent local sessions (default 5; use `--all` to show everything, `--dedupe` to keep the newest row per tool/cwd/summary).
  - `al sessions list [COUNT]` — explicit list with the same flags.
  - Repeatable `--host HOST` adds a remote host to query. `local` is reserved for the current machine. With `--host`, `al` runs `al sessions list` on each host over SSH and prints read-only results grouped under `== <host> ==`. It is not a sync: no files are copied. Hosts are queried in the order given; `--all`/`--dedupe`/COUNT are forwarded to each remote. Per-host deduplication only; there is no cross-host deduplication. The command continues after a failed host and exits nonzero if any host failed. Empty, whitespace-containing, control-character, or option-like host values are rejected. `--host` cannot be combined with `--paths`, `--picker`, or `--fzf`.
  - `al sessions search QUERY` — search the text of user/assistant messages in discovered sessions. Search is local-only. `--dedupe` and `--picker` change output style.
  - `al sessions convert SOURCE TARGET INPUT [OUTPUT]` (visible alias `migrate`) — read `INPUT` in the native format of `SOURCE` and write a `TARGET`-compatible export. If `OUTPUT` is omitted, the export is written to the target tool's native session location and the path is printed.
  - `al sessions fork SESSION_REF TARGET` — fork a session to another tool.
  - `al sessions open SESSION_REF TARGET` — reopen a session in the target tool.
  - `al sessions open|fork --print-command` — print the native command without launching the agent, then exit. A cross-tool fork may first write the forked export.
  - `al sessions sync SRC_OR_DST [DST] [--tool TOOL]... [--dry-run]` — synchronize session catalogs point-to-point. With one endpoint, the local catalog is uploaded to that endpoint. With two endpoints, the first is the source and the second is the destination; both cannot be `local`. `--tool` can be repeated to limit the transfer to specific source tools. This is a separate command from listing; it copies files and merges Codex history, while `al sessions --host` is read-only.
- `al sks` — local interactive fuzzy filter over the displayed fields (tool, time, session id, summary) followed by a target-tool picker. Requires `fzf` on PATH.
- `al skss QUERY...` — same picker flow, but the first fuzzy list is filtered by a local message-body search across user/assistant text. Requires `fzf` on PATH.
- `al omlo|pilo|rpilo|grolo|hyperlo|dolo|colo|cclo [...]` — launch the corresponding coding agent (OMP, Pi, Rpi, Grok, Hyper, Droid, Codex, Claude). Common launcher flags:
  - `--host HOST` — run on a remote host over SSH (requires the current directory to be inside a git repository).
  - `--wt NAME` — use a named git worktree on the remote host (requires `--host`).
  - `--tmux` — wrap the launch in tmux (Unix only).
  - `--session=ID` — pass a session selector to the underlying tool.
  - `--` — protects everything after the delimiter from being interpreted as launcher flags, forwarding it verbatim to the agent.

  On macOS, remote launchers map `/Users/<user>` to `/home/<user>`. Additional component-aware mappings can be supplied through `AL_REMOTE_PATH_MAPS` as an ordered JSON array of absolute source/destination pairs, for example `[["/Volumes/workspace","/srv/workspace"]]`. Every source and destination must be an absolute path; malformed configuration fails before SSH is invoked. The remote host must have `al` installed and on PATH.
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
| Source (export from) | `pi`, `omp`, `droid`, `codex`, `claude`, `grok` |
| Target (convert to / launch) | `pi`, `omp`, `droid`, `codex`, `claude`, `grok`, `hyper`, `rpi` |

`grok` and `hyper` share the same storage layout; a Grok session can be converted to Hyper in place, and Hyper targets reuse Grok's native format. `pi` and `rpi` likewise share Pi session storage; Rpi launches the `rpi` executable against that layout.

Session conversion is intentionally lossy:

- Only user/assistant messages and the first recognized text block from each message are preserved. Images, attachments, tool calls, and other metadata are dropped.
- Empty lines are skipped. After a successful native load, unparseable records and non-message entries may be skipped.
- Generated summaries normalize whitespace and are truncated to 100 characters; projected message text is preserved.
- `grok` and `hyper` targets reuse Grok's storage layout; `pi` and `rpi` targets reuse Pi's storage layout; other targets write their own native formats.

This is by design: `al` is a loader that passes the useful context forward, not a bit-perfect archival mirror.

## Source format note

Format adapters read each tool's native export:

- **Pi / OMP** — newline-delimited JSONL conversation trees.
- **Droid** — `session_start` and `message` typed records.
- **Codex / Claude / Grok** — tool-specific JSON/JSONL or directory layouts. Hyper targets reuse Grok's storage layout. Rpi targets reuse Pi's storage layout.

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
|-------|---------|
| macOS arm64 | `al-0.2.0-aarch64-apple-darwin.tar.gz` |
| macOS x86_64 | `al-0.2.0-x86_64-apple-darwin.tar.gz` |
| Linux x86_64 (glibc 2.31+) | `al-0.2.0-x86_64-unknown-linux-gnu.tar.gz` |
| Linux arm64 (glibc 2.31+) | `al-0.2.0-aarch64-unknown-linux-gnu.tar.gz` |
| Windows x86_64 | `al-0.2.0-x86_64-pc-windows-msvc.zip` |
| Checksums | `SHA256SUMS` |

The tag must match `Cargo.toml` version exactly (`v0.2.0` ↔ `0.2.0`) or the build fails.

## License

MIT. See [`LICENSE`](./LICENSE).
