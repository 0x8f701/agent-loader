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
curl -fsSL https://raw.githubusercontent.com/0x8f701/agent-loader/main/install.sh | bash -s -- --version v0.9.0
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

# Live tmux agents (not the session catalog: use `al sessions list` for that).
al list
al list --diff
al list --host host-a --host host-b
al attach
al attach omp
al attach --target %12
al attach --host host-a --host host-b
al watch
al watch --host host-a --host host-b --interval 5
al watch --no-git
al supervise
al supervise --host host-a --interval 5
al supervise diff --host host-a
al supervise send --host host-a --message "continue from GOAL.md"
al supervise send omp --message "continue from GOAL.md"

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

# Pick a live tmux agent pane and attach (requires fzf on PATH).
al attach
al attach omp
al attach --host host-a
al attach --target %12

# Point-to-point session catalog sync (dry-run first).
# Cursor Agent native SQLite stores are intentionally not synchronized.
al sessions sync host-a --dry-run
al sessions sync host-a host-b --tool omp --tool pi --dry-run

# Create or update a project, optionally on a remote host / worktree / agent.
al new sample-app "ship the parser"
al new sample-app "ship the parser" --tmux
al new host-a ~/Projects/sample-app "ship the parser" --executor omp --worktree
al new host-a ~/Projects/sample-app "ship the parser" --orchestrator pilo --executor omp --reviewer grok --worktree
al new ~/Projects/sample-app "ship the parser" --executor omp --print-command
al new sample-app "ship the parser" --git https://example.test/org/sample-app.git
al new workspace "ship both" --git https://example.test/org/alpha.git --git https://example.test/org/beta.git --worktree

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

- `al sessions [COUNT]` — list recent local sessions (default 5; use `--all` to show everything, `--dedupe` to keep the newest row per tool/cwd/summary). Workflow/subagent children are hidden unless `--children` is set. User forks that only carry `parentSession` stay visible.
  - `al sessions list [COUNT]` — explicit list with the same flags.
  - Repeatable `--host HOST` adds a remote host to query. `local` is reserved for the current machine. With `--host`, `al` runs `al sessions list` on each host over SSH and prints read-only results grouped under `== <host> ==`. It is not a sync: no files are copied. Hosts are queried in the order given; `--all`/`--dedupe`/`--children`/COUNT are forwarded to each remote. Per-host deduplication only; there is no cross-host deduplication. The command continues after a failed host and exits nonzero if any host failed. Empty, whitespace-containing, control-character, or option-like host values are rejected. `--host` cannot be combined with `--paths`, `--picker`, or `--fzf`.
  - `al sessions --fzf` / `al sessions list --fzf` — local interactive fuzzy filter over tool, time, session id, and summary, followed by a target picker that opens the session. Native Agent rows offer only `agent` and default to it. Requires `fzf` on PATH. Uses the full deduped catalog, not the default 5-row list.
  - `al sessions search QUERY` — search the text of user/assistant messages in discovered sessions. Search is local-only. `--dedupe` and `--picker` change output style. `--children` includes workflow/subagent sessions.
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
- `al new [HOST] NAME|PATH GOAL` — create the project if it is missing, otherwise update `GOAL.md`. A bare `NAME` goes under `$AL_PROJECTS_HOME` or `~/Projects` (remote: `$HOME/Projects`). A path must be absolute or start with `~/` on a remote host. Missing repos get `README.md`, `GOAL.md`, `git init`, and `Initial commit`; existing git checkouts commit `Set goal`. `--worktree [NAME]` (alias `--wt`) creates or reuses `~/Projects/<repo>-<NAME>` (`NAME` defaults to `wt`). Repeatable `--git URL` clones instead of `git init`: one URL goes to `~/Projects/<NAME>` (or `~/Projects/<worktree>` when `--worktree` is set); multiple URLs require `--worktree` and land under `~/Projects/worktree` (or `~/Projects/<NAME>`), each in a child directory named from the URL. A missing `--worktree` value with `--git` is `worktree`, not `wt`. Existing checkouts skip clone and only update `GOAL.md`. The parent of multiple clones is not a git repo; `GOAL.md` and `.al` role files live there and that folder is the launch cwd. `--executor|--orchestrator|--reviewer TOOL` start fresh agents (yolo/approval flags, no `--continue`) in **one tmux session** with one window per role. Window names are `<tool>-<project>` (for example `omlo-pi-zig-wt`); the session is the project/worktree name. Usual case is just `--executor omp`. Each role gets `.al/<role>.md` covering the coordinate envelope (`TASK`/`ACK`/`PROGRESS`/`DONE`/`BLOCKED`/`RELAY`/`AUDIT`/`INFO`, star topology, `modifying:`). After the TUI is up, grok/hyper/claude get `tmux send-keys` of `/goal <text>`; pi/omp get `/goal` then the text; codex/droid/agent have no `/goal` (files only). `--tmux` is implied by a role flag unless `--no-tmux` (multiple roles require tmux; `--no-tmux` cannot send `/goal`). Role launch and `--tmux` are Unix-only. Remote create/open uses SSH + `fish` + `git`. Opening copies this `al` to `/tmp/al-<version>` on the host instead of using the remote PATH binary. An existing exact tmux session name is refused, not replaced. `--print-command` prints the plan without writing files or running SSH.
- `al list [--host HOST]... [--json] [--diff]` — snapshot **live tmux coding-agent panes** (not `al sessions list`). Agents are identified from the pane process tree (including `node`/`bash` wrappers) and from `al` window names such as `omlo-sample-app`. Rows are grouped by worktree: a header with cwd, branch, and dirty `DIFF` when present (`+12/-3`, `?2` untracked; `-` if not a git repo), then fixed columns per pane (`state`, `agent`, `pane`, `session`, `idle`, `snippet`) with `-` placeholders for empty cells. TUI chrome, statuslines, and shell prompts are skipped. An empty scan prints `no live agents`. `--diff` also prints each unique worktree's `git diff HEAD` plus untracked files. State is `blocked`, `asking`, `working`, or `idle` from the current screen: structural signals (spinner, `❯ `, `esc to interrupt`) use the last 12 nonempty lines; word matches (`thinking`, `FAILED`) use the last 2. Repeatable `--host` SSHs a one-shot dump of tmux/ps/pane-tree cwds/captures/git in parallel; one failed host still prints the others, then exits nonzero. The remote host does not need a newer `al`. Unix tmux only.
- `al attach [--host HOST]... [QUERY...]` — pick a live pane with fzf and attach (`switch-client` when already inside tmux, `attach-session` otherwise; remote `--host` uses `ssh -tt`). Requires `fzf` on PATH unless `--target` is set. `--target` attaches directly to a pane id (`%12`), unique agent name, session/window name, or cwd without opening fzf. Trailing `QUERY...` seeds the fzf filter and conflicts with `--target`.
- `al watch [--host HOST]... [--interval SECS] [--no-git]` — refresh the live table in place across local and remote hosts. Multi-host scans run in parallel. The summary line shows counts plus any failed hosts. `--no-git` skips git status for a cheaper tick. It does **not** auto-reply.
- `al supervise [--host HOST]... [--interval SECS] [--no-git]` — compatibility watch (same table as `al watch`, labeled `al supervise`). Prefer `al watch` for monitoring. `al supervise diff [TARGET]` prints one worktree's git diff (same target rules as send). `al supervise send [TARGET] --message TEXT` pastes into a pane. Omit `TARGET` (or use `attention`) to send to the first blocked pane, else the first asking pane. `TARGET` can be a pane id (`%12`), a unique agent name (`omp`), a session/window name, or a cwd. `--host` selects the machine. `--no-submit` pastes without Enter.
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

`grok` and `hyper` share the same storage layout; a Grok session can be converted to Hyper in place, and Hyper targets reuse Grok's native format. Rpi is a separate catalog at `~/.rpi/sessions`; the on-disk JSONL is Pi-compatible, so `al` can open a Pi session with `rpi` (and the reverse) without converting. When emitting to `rpi`, `tool_result` blocks that arrived inside a user turn (Claude-style) are written as native top-level `toolResult` records; leftover user text and images stay on the user turn.

Cursor Agent discovery reads `.cursor/chats/<32-hex-workspace-hash>/<session-id>/store.db` exactly two directories beneath the chats root, plus the sibling `meta.json` sidecar for cwd, title, and timestamps. The SQLite database is opened read-only. Cursor's store format is undocumented and `al` never emits or converts sessions into it; malformed individual stores are skipped without hiding other catalog rows.

The managed `rpi` executable is resolved from `$PI_HOME/bin/rpi`, then
`~/.rpi/bin/rpi`, then `PATH`. Rpi sessions are discovered under `~/.rpi/sessions`.

Session conversion preserves the portable conversation:

- User, assistant, and tool turns keep every recognized text, thinking, tool-use, and tool-result block, remapped into the target tool's native record shape.
- Images are remapped to each target's native image block (Pi/OMP `image`, Claude/Droid `image`+`source`, Codex `input_image`/`images[]`, Grok `image`+`url`).
- Compaction summaries become native Pi/OMP `compaction` records or Claude `isCompactSummary` user turns; other notes stay Pi/OMP `custom_message` records or Claude attachments. Other targets keep the note text.
- Source model, provider, and thinking level are written back when the target format has a place for them (Pi/OMP `thinking_level_change`, Claude `effort`, Codex `reasoning_effort`, Grok `reasoning_effort`, Droid `thinkingLevel`).
- Session metadata that has no target equivalent (Claude attachments, Codex world_state, Grok hooks, todos, labels) is still dropped.
- Empty lines are skipped. After a successful native load, unparseable records and non-message entries may be skipped.
- Generated summaries normalize whitespace and are truncated to 100 characters; projected message text is preserved.
- `grok` and `hyper` targets reuse Grok's storage layout; `pi` writes `~/.pi/agent/sessions` and `rpi` writes `~/.rpi/sessions` using the same JSONL tree. Rpi additionally splits user-embedded tool results into `role: "toolResult"` records so the native journal reader does not drop them.

Cursor Agent remains parse/search/open only: its SQLite blob store is not a conversion target.

## Source format note

Format adapters read each tool's native export:

- **Pi / OMP** — newline-delimited JSONL conversation trees, including thinking, `toolCall` / `toolResult`, visible `custom_message`, `branch_summary`, `bashExecution`, and compaction notes.
- **Droid** — `session_start` and `message` typed records with `text` / `thinking` / `tool_use` / `tool_result` / `image` blocks.
- **Codex** — rollout JSONL `response_item` messages plus `function_call`, `function_call_output`, `reasoning`, `compacted`, and user `images`.
- **Claude** — UUID conversation graph; user/assistant content blocks including thinking, tools, images, and attachments.
- **Grok / Hyper** — `summary.json` plus `chat_history.jsonl`, falling back to ACP `updates.jsonl`; tool, reasoning, `backend_tool_call`, and image records (including tool-result `images`) are kept.
- **Cursor Agent** — a read-only SQLite `store.db` adapter with sibling `meta.json`; plaintext user/assistant/tool text is projected, injected wrappers are excluded, and the native store remains canonical.

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

As a library, `domain` / `formats` / `emit` build with `--no-default-features` and do not pull in `clap` or `rusqlite`. The `al` binary enables the default `cli` feature, which also turns on `catalog` (bundled SQLite for Cursor Agent stores).

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
| macOS arm64 | `al-0.9.0-aarch64-apple-darwin.tar.gz` |
| macOS x86_64 | `al-0.9.0-x86_64-apple-darwin.tar.gz` |
| Linux x86_64 (glibc 2.31+) | `al-0.9.0-x86_64-unknown-linux-gnu.tar.gz` |
| Linux arm64 (glibc 2.31+) | `al-0.9.0-aarch64-unknown-linux-gnu.tar.gz` |
| Windows x86_64 | `al-0.9.0-x86_64-pc-windows-msvc.zip` |
| Checksums | `SHA256SUMS` |

The tag must match `Cargo.toml` version exactly (`v0.9.0` ↔ `0.9.0`) or the build fails.

## License

MIT. See [`LICENSE`](./LICENSE).
