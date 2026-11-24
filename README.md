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
curl -fsSL https://raw.githubusercontent.com/0x8f701/agent-loader/main/install.sh | bash -s -- --version v0.12.0
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
al attach --all
al attach --all --host host-a --host host-b
al attach --host host-a --host host-b
al watch
al watch --host host-a --host host-b --interval 5
al watch --no-git
al supervise
al supervise --host host-a --interval 5
al supervise --session agentlo-demo
al supervise agentlo-demo --host host-a --interval 5
al supervise omp --session demo-app
al supervise diff --host host-a
al supervise send --host host-a --message "continue from GOAL.md"
al supervise send --session demo-app --message "continue from GOAL.md"
al supervise send omp --message "continue from GOAL.md"

# Convert a Pi export into a Claude-compatible export.
al sessions convert pi claude /workspace/project/pi-session.jsonl /workspace/project/claude-session.jsonl

# Relocate sessions from one directory to another (catalog folder or recorded cwd).
al sessions move /workspace/old-project /workspace/new-project
al sessions move ~/.pi/agent/sessions/--old-project-- ~/.pi/agent/sessions/--new-project--

# Convert a Grok session for OMP (the `migrate` spelling is a visible alias).
al sessions migrate grok omp /workspace/project/grok-session.jsonl /workspace/project/omp-session.jsonl

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
al attach --all
al attach --all --host host-a --host host-b

# Point-to-point session catalog sync (dry-run first).
# Cursor Agent native SQLite stores are intentionally not synchronized.
# Antigravity sync needs Python 3 and its standard-library sqlite3 module
# on every participating endpoint. It snapshots conversation databases
# (committed WAL included) and copies matching brain directories. It does
# not copy the whole Antigravity root, credentials, or settings.
al sessions sync host-a --dry-run
al sessions sync host-a host-b --tool omp --tool pi --dry-run
al sessions sync host-a host-b --tool antigravity --dry-run

# Create or update a project, optionally on a remote host / worktree / agent.
al new sample-app "ship the parser"
al new sample-app "ship the parser" --tmux
al new host-a ~/Projects/sample-app "ship the parser" --executor omp --worktree
al new host-a ~/Projects/sample-app "ship the parser" --orchestrator pilo --executor omp --reviewer grok --worktree
al new ~/Projects/sample-app "ship the parser" --executor omp --print-command
al new sample-app "ship the parser" --executor agyo
al new sample-app "ship the parser" --git https://example.test/org/sample-app.git
al new workspace "ship both" --git https://example.test/org/alpha.git --git https://example.test/org/beta.git --worktree

# Launch Cursor's official `agent` CLI. Bare `agentlo` tries to continue
# Cursor's latest chat, then starts a new chat if `--continue` exits nonzero.
al agentlo
al agentlo chat-123 "fix the parser"
al agentlo --session chat-123 "fix the parser"
al agentlo --tmux
al agentlo --worktree feature-name
# Launch Antigravity's `agy` CLI. Bare `agyo` resumes the most recent
# conversation for the active workspace (`agy --continue`). If that
# conversation is missing or deleted, `agy` starts a fresh session.
# `agy` has no `--fork`. These clone a conversation, then resume the copy.
al agyo
al agyo --tmux
al agyo --session conv-1 "fix the parser"
al agyo --fork
al agyo --fork conv-1
al agyo --session conv-1 --fork
al agyo models
```

## Commands

Run `al --help` and `al COMMAND --help` for the current argument surface.

Routine catalog and picker skips are debug logs, quiet by default. Use `RUST_LOG=agent_loader=debug al sessions --all` to inspect them; `RUST_LOG=off` disables logs. Filters support module-specific levels, such as `RUST_LOG=debug,agent_loader::sessions=off`. Logs use stderr; session rows use stdout. Invalid arguments and failed commands still report errors and return nonzero regardless of the log filter. Remote commands use the remote logging configuration; local `RUST_LOG` is not forwarded.

- `al sessions [COUNT]` — list recent local sessions (default 5; use `--all` to show everything, `--dedupe` to keep the newest row per tool/cwd/summary). Workflow/subagent children are hidden unless `--children` is set. User forks that only carry `parentSession` stay visible.
  - Claude transcripts containing only sidechain conversation records are children too. Mixed transcripts remain visible, but their sidechain messages are excluded from the active conversation.
- Generated temporary workspaces are hidden by default. `--temporary` includes a session when the first directory of its cwd under `/tmp`, `/var/tmp`, or `/private/tmp` is a generated scratch name: `.tmp` followed by at least six ASCII alphanumeric characters, or exactly one of the Pi scratch prefixes `pi-session-persist-now-`, `pi-session-custom-omit-`, or `pi-session-custom-` followed by a canonical UUID (8-4-4-4-12 hex digits with hyphens at those positions, case-insensitive, and nothing after it — truncated, unhypenated, or trailing-junk names stay visible). Everything nested below a matching directory is temporary as well; a `..` component anywhere in the cwd disables the rule; ordinary named projects under those roots, scratch-shaped names deeper inside a durable project, and paths outside the temporary roots stay visible. This is an explicit allowlist of observed scratch shapes, not a claim about Pi's directory generator. A temporary child needs both `--temporary` and `--children`; list, search, `--fzf`, and query honor both flags. Direct session references remain resolvable.
  - `al sessions list [COUNT]` — explicit list with the same flags.
  - Repeatable `--host HOST` adds a remote host to query. `local` is reserved for the current machine. With `--host`, `al` runs `al sessions list` on each host over SSH and prints read-only results grouped under `== <host> ==`. It is not a sync: no files are copied. Hosts are queried in the order given; `--all`/`--dedupe`/`--children`/COUNT are forwarded to each remote. Per-host deduplication only; there is no cross-host deduplication. The command continues after a failed host and exits nonzero if any host failed. Empty, whitespace-containing, control-character, or option-like host values are rejected. `--host` cannot be combined with `--paths`, `--picker`, or `--fzf`.
    `--temporary` is also forwarded; the remote host must run 0.11.0 or newer. 0.11.0 introduced temporary-cwd filtering, covering both `.tmp…` names and the Pi scratch names above; releases 0.10.0 and earlier list both kinds and do not support `--temporary`.
  - `al sessions --fzf` / `al sessions list --fzf` — local interactive fuzzy filter over tool, time, session id, and summary, followed by a target picker that opens the session. Native Agent rows offer only `agent` and default to it. Requires `fzf` on PATH. Uses the full deduped catalog, not the default 5-row list.
  - `al sessions search QUERY` — search the text of user/assistant messages in discovered sessions. Search is local-only. `--dedupe` and `--picker` change output style. `--children` includes workflow/subagent sessions.
  - `al sessions query QUERY...` — the same picker-and-open flow as `--fzf`, after a local user/assistant message-body search, including parsed native Agent messages. Requires `fzf` on PATH.
  - `al sessions convert SOURCE TARGET INPUT [OUTPUT]` (visible alias `migrate`) — read `INPUT` in the native format of `SOURCE` and write a `TARGET`-compatible export. If `OUTPUT` is omitted, the export is written to the target tool's native session location and the path is printed. `antigravity` and `agy` name Antigravity. Cursor Agent is intentionally excluded because its store format is undocumented. Portable conversion into or out of Antigravity is not a byte-for-byte protobuf copy.
  - `al sessions move FROM TO [--tool TOOL]... [--dry-run]` — move native session files from one directory to another without converting them. `FROM` can be a catalog folder (match by file path) or a recorded workspace path (match by `cwd`, including after the project directory itself is gone). `TO` can be another catalog folder, a dump directory, or the new workspace: matching `cwd` values are rewritten and files are re-homed to that tool's native layout. For Antigravity, a destination inside `~/.gemini/antigravity-cli/conversations` (that directory or one beneath it) places `<uuid>.db` there. If that database's parent directory is named `conversations`, the brain companion is `~/.gemini/antigravity-cli/brain/<uuid>` and the native summary is registered; any other parent gets `<parent>/brain/<uuid>` and no native registration. A destination outside that catalog is the new workspace, not a dump to an arbitrary directory: the database stays at `conversations/<uuid>.db` and its workspace URI is rewritten. Stop `agy` and checkpoint the database first: a `-wal` or `-shm` sidecar refuses the move. Cursor Agent stores are excluded. The command refuses to overwrite an existing destination, to move an entire home/catalog root, or to delete a Grok directory that contains unexpected files.
  - `al sessions fork SESSION_REF TARGET` — fork a session to another tool. Agent is not a fork target and native Agent sessions cannot be forked. Forking to `antigravity` writes a new conversation database and resumes that copy with `agy --conversation <new-id>` in the recorded cwd. `agy` has no fork flag; the fork is the emitted carrier, not a native fork command.
  - `al sessions open SESSION_REF TARGET` — reopen a session in the target tool. A native Agent session may only target `agent`; it runs `agent --force --trust --approve-mcps --resume <session-id>` in the recorded cwd. Opening as `antigravity` runs `agy --conversation <id>` in the recorded cwd.
  - `al sessions open|fork --print-command` — print the native command without launching the agent, then exit. A cross-tool fork may first write the forked export.
  - `al sessions sync SRC_OR_DST [DST] [--tool TOOL]... [--dry-run]` — synchronize supported session catalogs point-to-point. With one endpoint, the local catalog is uploaded to that endpoint. With two endpoints, the first is the source and the second is the destination; both cannot be `local`. `--tool` can be repeated to limit the transfer to specific source tools (`antigravity` or `agy` selects Antigravity). Cursor Agent SQLite stores are excluded and `--tool agent` is rejected. Antigravity is not an rsync of `~/.gemini/antigravity-cli`: both endpoints run one fixed Python 3 program that uses only the standard-library `sqlite3` module and does not call `fcntl`. It inventories `conversations/<uuid>.db` and `brain/<uuid>/`, then transfers a WAL-consistent SQLite snapshot plus those brain files. Missing `python3` or `sqlite3` fails that tool. Credentials and settings are not copied. A transferred conversation merges its summary row and its workspace entry in `cache/last_conversations.json` without replacing unrelated entries. Convert, sync, and move take `~/.gemini/antigravity-cli/cache/.al-routing.lock` exclusively while writing that routing and fail closed if the lock already exists: the continue cache is busy. After a killed writer, remove that file only once no `al` convert, sync, or move is writing native routing. `al` does not take over a stale lock. This is separate from read-only multi-host listing.
- `al omlo|pilo|rpilo|grolo|dolo|colo|cclo|agentlo|agyo [...]` — launch the corresponding coding agent (OMP, Pi, Rpi, Grok, Droid, Codex, Claude, Cursor Agent, Antigravity). Common launcher flags:
  - `--host HOST` — run on a remote host (mosh when local `mosh` and remote `mosh-server` both exist, otherwise ssh; `AL_REMOTE=mosh|ssh` forces one and skips the probe). Requires the current directory to be inside a git repository.
  - `--wt NAME` — use a named git worktree on the remote host (requires `--host`).
  - `--tmux` — wrap the launch in tmux (Unix only).
  - `--session=ID` — pass a session selector to the underlying tool.
  - `--` — protects everything after the delimiter from being interpreted as launcher flags, forwarding it verbatim to the agent.

  On macOS, remote launchers map `/Users/<user>` to `/home/<user>`. Additional component-aware mappings can be supplied through `AL_REMOTE_PATH_MAPS` as an ordered JSON array of absolute source/destination pairs, for example `[["/Volumes/workspace","/srv/workspace"]]`. Every source and destination must be an absolute path; malformed configuration fails before the remote tool is invoked. The remote host must have `al` installed and on PATH.

  `al agentlo` launches Cursor's official `agent` CLI. With no tool args it first runs `agent --force --trust --approve-mcps --continue`; if that command exits nonzero, it retries as `agent --force --trust --approve-mcps` to create a new chat. The continue probe hides Cursor's "No previous chats found." status line, including inside `--tmux`. Cursor's native local worktree options (`-w`/`--worktree [NAME]` and `--worktree-base REF`) pass through normally. The launcher-level `--wt NAME` remains the remote-host worktree control and therefore requires `--host`; `--tmux` works for both local and remote launches. A `--session ID` selector (or a positional chat id) maps to `--resume ID`; any other arguments are forwarded verbatim after the base approval flags. Separately, `al sessions`, `al sessions search`, `al sessions --fzf`, and `al sessions query` discover native Cursor Agent sessions and can reopen them exactly; conversion and sync remain disabled because the native SQLite/blob format is undocumented and live stores may depend on WAL state.
  `al agyo` launches Antigravity's official `agy` CLI. With no tool args it runs `agy --continue` in the current working directory, which resumes the most recent conversation for that workspace. If that conversation is missing or was deleted, `agy` starts a fresh session. `al` does not retry or hide a failed continue. `--tmux` wraps that command. `--session ID` and `--session=ID` map to `--conversation ID` and keep the remaining arguments; a positional id is not treated as a conversation id. `agy` has no `--fork` flag. `al agyo --fork [ID]` and `al agyo --session ID --fork` clone the selected native conversation into a fresh `conversations/<new-uuid>.db` plus `brain/<new-uuid>/`, then run `agy --conversation <new-uuid>`. With no ID, `--fork` reads the current workspace's entry in `~/.gemini/antigravity-cli/cache/last_conversations.json`. `--fork` is never passed through to `agy`. Other native options and subcommands (for example `--model`, `models`, and `mcp`) are forwarded unchanged. The regular launcher does not inject `--dangerously-skip-permissions`. `al new --executor agyo` (also `agy` or `antigravity`) starts fresh: `agy --dangerously-skip-permissions` with no `--continue`, then `/goal <objective>`. `al rpilo --fork` with no session selects the newest JSONL under `~/.rpi/sessions` and runs `rpi --fork <path>`; it does not read the Pi catalog. `al pilo --fork` with no session still uses the Pi root (`$PI_CODING_AGENT_DIR/sessions`, or `~/.pi/agent/sessions`).
- `al new [HOST] NAME|PATH GOAL` — create the project if it is missing, otherwise update `GOAL.md`. A bare `NAME` goes under `$AL_PROJECTS_HOME` or `~/Projects` (remote: `$HOME/Projects`). A path must be absolute or start with `~/` on a remote host. Missing repos get `README.md`, `GOAL.md`, `git init`, and `Initial commit`; existing git checkouts commit `Set goal`. `--worktree [NAME]` (alias `--wt`) creates or reuses `~/Projects/<repo>-<NAME>` (`NAME` defaults to `wt`). Repeatable `--git URL` clones instead of `git init`: one URL goes to `~/Projects/<NAME>` (or `~/Projects/<worktree>` when `--worktree` is set); multiple URLs require `--worktree` and land under `~/Projects/worktree` (or `~/Projects/<NAME>`), each in a child directory named from the URL. A missing `--worktree` value with `--git` is `worktree`, not `wt`. Existing checkouts skip clone and only update `GOAL.md`. The parent of multiple clones is not a git repo; `GOAL.md` and `.al` role files live there and that folder is the launch cwd. `--executor|--orchestrator|--reviewer TOOL` start fresh agents (yolo/approval flags, no `--continue`) in **one tmux session** with one window per role. Window names are `<tool>-<project>` (for example `omlo-pi-zig-wt`); the session is the project/worktree name. Usual case is just `--executor omp`. Each role gets `.al/<role>.md` covering the coordinate envelope (`TASK`/`ACK`/`PROGRESS`/`DONE`/`BLOCKED`/`RELAY`/`AUDIT`/`INFO`, star topology, `modifying:`). After the TUI is up, grok/claude/agy get `tmux send-keys` of `/goal <text>`; pi/omp get `/goal` then the text; codex/droid/agent have no `/goal` (files only). `--tmux` is implied by a role flag unless `--no-tmux` (multiple roles require tmux; `--no-tmux` cannot send `/goal`). Role launch and `--tmux` are Unix-only. Remote create/open uses mosh when local `mosh` and remote `mosh-server` both exist, otherwise ssh, plus `fish` + `git`. Opening copies this `al` to `/tmp/al-<version>` on the host instead of using the remote PATH binary. An existing exact tmux session name is refused, not replaced. `--print-command` prints the plan without writing files or probing/running a remote shell; the bootstrap line is mosh when the local client exists (or `AL_REMOTE` forces one), and the real create/open still checks remote `mosh-server`.
- `al list [--host HOST]... [--json] [--diff]` — snapshot **live tmux coding-agent panes** (not `al sessions list`). Agents are identified from the pane process tree (including `node`/`bash` wrappers). `agy` and `agyo` are Antigravity, including when a runtime wraps them; an IDE process named `antigravity` is not. A pane whose agent has exited (only a shell left, even if the window is still named `omlo-sample-app`) is omitted. Rows are grouped by worktree: a header with cwd, branch, and dirty `DIFF` when present (`+12/-3`, `?2` untracked; `-` if not a git repo), then fixed columns per pane (`state`, `agent`, `pane`, `session`, `idle`, `snippet`) with `-` placeholders for empty cells. TUI chrome, statuslines, and shell prompts are skipped. An empty scan prints `no live agents`. `--diff` also prints each unique worktree's `git diff HEAD` plus untracked files. State is `blocked`, `asking`, `working`, or `idle` from the current screen: structural signals (spinner, `❯ `, `esc to interrupt`) use the last 12 nonempty lines; word matches (`thinking`, `FAILED`) use the last 2. Repeatable `--host` dumps tmux/ps/pane-tree cwds/captures/git in parallel over mosh when local `mosh` and remote `mosh-server` both exist, otherwise ssh; one failed host still prints the others, then exits nonzero. The remote host does not need a newer `al`. Unix tmux only.
- `al attach [--host HOST]... [QUERY...]` — pick a live pane with fzf and attach (`switch-client` when already inside tmux, `attach-session` otherwise; remote `--host` uses mosh when local `mosh` and remote `mosh-server` both exist, otherwise `ssh -tt`). `AL_REMOTE=mosh|ssh` forces the tool and skips the probe. Requires `fzf` on PATH unless `--target` or `--all` is set. `--target` attaches directly to a pane id (`%12`), unique agent name, session/window name, or cwd without opening fzf. `--all` creates one local tmux session named after each host, with one window per live tmux session on that host (`env -u TMUX tmux attach` locally, mosh/ssh + `tmux attach` remotely), then attaches to the first host session. Only panes whose agent process is still running are included; a leftover `omlo-*` window with just a shell is skipped. Trailing `QUERY...` seeds the fzf filter and conflicts with `--target` and `--all`.
  - `al attach --all --host host-a` reuses its marked local aggregate and adds missing windows. Existing windows, renamed aggregates, and unrelated sessions are preserved. Name collisions receive numeric suffixes; host-name punctuation is escaped so distinct host names do not collapse into one name. This attaches running agents, not saved transcripts.
- `al watch [--host HOST]... [--interval SECS] [--no-git]` — refresh the live table in place across local and remote hosts. Multi-host scans run in parallel. The summary line shows counts plus any failed hosts. `--no-git` skips git status for a cheaper tick. It does **not** auto-reply.
- `al supervise [TARGET] [--session NAME] [--host HOST]... [--interval SECS] [--no-git] [--message TEXT]` — nudge idle panes so they keep running. Bare `al supervise` keeps **every** live pane. `--session NAME` (or a `TARGET` that is that tmux session's name) keeps every live pane in that tmux session only. Combined with a pane/agent/window/cwd `TARGET`, keep is pinned to that one pane. Working, asking, and picker panes are left alone. `--message` defaults to continuing from `GOAL.md`. Submit uses tmux `C-m` (not `Enter`): leftover `[Pasted text` is cleared with Ctrl-U, then keep retries `C-m` until the follow-up box is no longer holding the nudge. Use `al watch` for the live table. `al supervise diff [TARGET] [--session NAME]` prints one worktree's git diff. `al supervise send [TARGET] [--session NAME] --message TEXT` is a one-shot paste. A session name (flag or `TARGET`) sends to the first blocked pane in that session, else the first asking pane, else the first pane. Omit send's `TARGET` (or use `attention`) for the same attention order on the whole host. `--host` selects the machine. `--no-submit` pastes without Enter.

## Supported tools and conversion behavior

`al` recognizes the following tools:

| Role | Tools |
|------|-------|
| Source (discover/search) | `pi`, `rpi`, `omp`, `droid`, `codex`, `claude`, `grok`, `agent`, `antigravity` |
| Target (convert to / launch) | `pi`, `rpi`, `omp`, `droid`, `codex`, `claude`, `grok`, `antigravity`; `agent` is open-only for native Agent sessions |

Rpi is a separate catalog at `~/.rpi/sessions`; the on-disk JSONL is Pi-compatible, so `al` can open a Pi session with `rpi` (and the reverse) without converting. When emitting to `rpi`, `tool_result` blocks that arrived inside a user turn (Claude-style) are written as native top-level `toolResult` records; leftover user text and images stay on the user turn.

Antigravity discovery reads only `~/.gemini/antigravity-cli/conversations/<uuid>.db`, a canonical-UUID SQLite conversation opened read-only so a live WAL stays visible. Arbitrary SQLite files, nested databases, and the rest of `antigravity-cli` (credentials, settings, summaries) are not catalog rows. Search uses decoded user, assistant, thinking, and tool text.

Cursor Agent discovery reads `.cursor/chats/<32-hex-workspace-hash>/<session-id>/store.db` exactly two directories beneath the chats root, plus the sibling `meta.json` sidecar for cwd, title, and timestamps. The SQLite database is opened read-only. Cursor's store format is undocumented and `al` never emits or converts sessions into it; malformed individual stores are skipped without hiding other catalog rows.

The managed `rpi` executable is resolved from `$PI_HOME/bin/rpi`, then
`~/.rpi/bin/rpi`, then `PATH`. Rpi sessions are discovered under `~/.rpi/sessions`.

Session conversion preserves the portable conversation:

- User, assistant, and tool turns keep every recognized text, thinking, tool-use, and tool-result block, remapped into the target tool's native record shape.
- Images are remapped to each target's native image block (Pi/OMP `image`, Claude/Droid `image`+`source`, Codex `input_image`/`images[]`, Grok `image`+`url`).
- Compaction summaries become native Pi/OMP `compaction` records or Claude `isCompactSummary` user turns; other notes stay Pi/OMP `custom_message` records or Claude attachments. Other targets keep the note text.
- Codex developer notes remain native developer records; Droid todo notes remain native `todo_state` records. Other cross-format notes may become ordinary text rather than retaining their original record type.
- Source model, provider, and thinking level are written back when the target format has a place for them (Pi/OMP `thinking_level_change`, Claude `effort`, Codex `reasoning_effort`, Grok `reasoning_effort`, Droid `thinkingLevel`).
- Session metadata that has no target equivalent (Claude attachments, Codex world_state, Grok hooks, todos, labels) is still dropped.
- Conversion is not a byte-for-byte archive: encrypted reasoning cannot be transferred across providers, and Codex tool-result output does not encode the portable error flag. Thinking signatures and provider metadata survive only where the native format supports them. Antigravity's native store is SQLite plus protobuf and portable conversion is not a lossless protobuf copy. It keeps recognized text, thinking, tool calls, tool results, and inline images on both user and assistant turns. Media that is only a URI becomes a note that preserves that URI; inline bytes are not invented for it. Binary thinking signatures are kept with an explicit `base64:` prefix and are neither treated as UTF-8 nor fabricated. Unknown protobuf fields, executor or generator metadata, and bookkeeping steps are dropped. A stored numeric model stays `model-N`; a foreign model name is omitted instead of being mapped to a native enum. Missing model or thinking settings may be filled from the target runtime.
- Empty lines are skipped. After a successful native load, unparseable records and non-message entries may be skipped.
- Generated summaries normalize whitespace and are truncated to 100 characters; projected message text is preserved.
- `grok` writes `~/.grok/sessions`; `pi` writes `~/.pi/agent/sessions` and `rpi` writes `~/.rpi/sessions` using the same JSONL tree. Rpi additionally splits user-embedded tool results into `role: "toolResult"` records so the native journal reader does not drop them.

Cursor Agent remains parse/search/open only: its SQLite blob store is not a conversion target. Antigravity is a real catalog source and conversion target; the portable session is not a lossless protobuf round trip.

## Source format note

Format adapters read each tool's native export:

- **Pi / OMP** — newline-delimited JSONL conversation trees, including thinking, `toolCall` / `toolResult`, visible `custom_message`, `branch_summary`, `bashExecution`, and compaction notes.
- **Droid** — `session_start` and `message` typed records with `text` / `thinking` / `tool_use` / `tool_result` / `image` blocks.
- **Codex** — rollout JSONL `response_item` messages plus `function_call`, `function_call_output`, `reasoning`, `compacted`, and user `images`.
- **Claude** — UUID conversation graph; user/assistant content blocks including thinking, tools, images, and attachments.
- **Grok** — `summary.json` plus `chat_history.jsonl`, falling back to ACP `updates.jsonl`; tool, reasoning, `backend_tool_call`, and image records (including tool-result `images`) are kept.
- **Cursor Agent** — a read-only SQLite `store.db` adapter with sibling `meta.json`; plaintext user/assistant/tool text is projected, injected wrappers are excluded, and the native store remains canonical.
- **Antigravity** — `conversations/<uuid>.db` under `~/.gemini/antigravity-cli`: a SQLite trajectory whose step payloads are protobuf. User and assistant text, thinking, tool calls, tool results, and inline image bytes are projected. URI-only media becomes a note that keeps the URI. Binary thinking signatures use a `base64:` prefix. Bookkeeping steps are not conversation text.

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

As a library, `domain` / `formats` / `emit` build with `--no-default-features` and do not pull in `clap` or `rusqlite`. The `al` binary enables the default `cli` feature, which also turns on `catalog` (bundled SQLite for Cursor Agent stores and Antigravity conversation databases). Antigravity catalog sync additionally needs Python 3 with its standard-library `sqlite3` module on each endpoint; that interpreter is not bundled.

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

Release builds remap build-machine paths out of the binary (`--remap-path-prefix` for the checkout and the cargo/rustup homes), and CI fails if any build-machine path prefix survives in the raw bytes of a release binary.

Workflow: [`.github/workflows/release.yml`](.github/workflows/release.yml)

### Artifacts

| Asset | Example |
| macOS arm64 | `al-0.12.0-aarch64-apple-darwin.tar.gz` |
| macOS x86_64 | `al-0.12.0-x86_64-apple-darwin.tar.gz` |
| Linux x86_64 (glibc 2.31+) | `al-0.12.0-x86_64-unknown-linux-gnu.tar.gz` |
| Linux arm64 (glibc 2.31+) | `al-0.12.0-aarch64-unknown-linux-gnu.tar.gz` |
| Windows x86_64 | `al-0.12.0-x86_64-pc-windows-msvc.zip` |
| Checksums | `SHA256SUMS` |

The tag must match `Cargo.toml` version exactly (`v0.12.0` ↔ `0.12.0`) or the build fails.

### 0.12.0

Local preparation only. This version is not tagged or published. `origin/main` was already current; do not treat these notes as a GitHub Release.

Antigravity (`agy`) is a catalog source, conversion target, and launcher.

- Native store: `~/.gemini/antigravity-cli/conversations/<uuid>.db`, SQLite with protobuf step payloads. Listing opens the database read-only. The Cursor Agent store stays excluded from convert, move, and sync.
- `al agyo` with no tool arguments runs `agy --continue` for the current working directory and starts a new conversation when none exists. `al agyo --session ID` maps to `agy --conversation ID`. `al agyo --fork [ID]` and `al agyo --session ID --fork` clone the conversation into a fresh carrier and then run `agy --conversation <new-id>`; bare `--fork` uses `cache/last_conversations.json` for the current workspace. `--fork` is not passed to `agy`. Other `agy` options and subcommands pass through unchanged.
- `al new --executor agyo` starts `agy --dangerously-skip-permissions` with no `--continue`, then sends `/goal`.
- `al sessions sync --tool antigravity` transfers WAL-consistent SQLite snapshots and matching `brain/<uuid>/` directories through one Python 3 program that uses only the standard-library `sqlite3` module and does not call `fcntl`. It does not rsync the Antigravity application root.
- Convert, sync, and move fail closed when `~/.gemini/antigravity-cli/cache/.al-routing.lock` already exists (the continue cache is busy). After a killed writer, remove that file only once no `al` convert, sync, or move is writing native routing. `al` does not take over a stale lock.
- `al sessions move` places an Antigravity `<uuid>.db` inside the conversations catalog. The brain companion is `~/.gemini/antigravity-cli/brain/<uuid>` when the parent directory is named `conversations`, and `<parent>/brain/<uuid>` otherwise; native summary registration runs only for the first layout. A destination outside that catalog rewrites the workspace URI in place and does not relocate the database. Stop `agy` and checkpoint first: a `-wal` or `-shm` sidecar refuses the move.
- `al rpilo --fork` with no session id reads the newest JSONL under `~/.rpi/sessions`. It no longer reads the Pi session root. `al pilo --fork` with no id still does.
- Portable conversion is not a lossless protobuf copy. Inline user and assistant images are kept; URI-only media becomes a note that preserves the URI. Binary thinking signatures use a `base64:` prefix. Numeric `model-N` is retained and foreign model names are omitted. `al sessions open … antigravity` runs `agy --conversation <id>`. `al sessions fork … antigravity` emits a new conversation carrier and resumes that new id; it does not call an `agy` fork flag.
- `al dolo --session ID` runs `droid --settings ~/.factory/settings.json --auto high --resume ID` because `droid` has no `--session`; `al dolo --session ID --fork` uses `droid --fork ID` after those settings. `al omlo --session ID --fork` stays `omp --fork <id>`.
- On Rust 1.85, the library suite passed 607 tests, the binary suite passed 46, and the 9-source by 8-target conversion matrix passed all 4 tests, including Agent as a source and native text and images. On local Linux x86_64, Antigravity CLI 1.5.16 resumed a synthetic Pi-to-Antigravity conversation with `agy --conversation` and displayed its history; `al agyo --tmux` on that cwd loaded the same history. An Antigravity sync copied the database, brain directory, and summary row, and the reverse copied nothing. The local `release-dist` build is stripped `al` 0.12.0, 3798720 bytes, with no debug or build-machine paths, packaged as `al-0.12.0-x86_64-unknown-linux-gnu.tar.gz` with `al` and `LICENSE`. The other four release targets were not built on this machine. Nothing is pushed or tagged.

## License

MIT. See [`LICENSE`](./LICENSE).
