# ai-audit Project Guidelines

## Purpose

CLI tool to audit and monitor AI assistant sessions. Supports Claude
Code, OpenCode, and pi (badlogic/pi-mono).

## Architecture

### Modular Structure

```
src/
├── main.rs              # Minimal entry point
├── lib.rs               # Common types, data dir helpers
├── cli/
│   ├── mod.rs           # CLI orchestrator
│   ├── def.rs           # Clap argument definitions
│   └── action/
│       ├── mod.rs       # Command dispatcher + resolve_session()
│       ├── list_sessions.rs
│       ├── permissions.rs
│       ├── transcript.rs
│       ├── activity.rs
│       └── rate.rs
├── claudecode/
│   ├── mod.rs           # Data dirs, session file resolution
│   ├── session.rs       # JSONL parsing, text search, tool_use extraction
│   ├── transcript.rs    # Transcript parser (JSONL → TranscriptEntry)
│   └── permissions.rs   # Permission event parsing from debug logs
├── opencode/
│   ├── mod.rs           # Session listing, text search, session info
│   ├── transcript.rs    # Transcript parser (message/part → TranscriptEntry)
│   ├── permissions.rs   # Permission parsing from part files + logs
│   ├── run.rs           # Agent invocation (for rate command)
│   └── cache.rs         # Caching support
├── pi/
│   ├── mod.rs           # Data dir, sessions dir, PiProvider impl
│   ├── session.rs       # JSONL parsing, text search, tool-call extraction
│   └── transcript.rs    # Transcript parser (JSONL → TranscriptEntry)
├── session_detect.rs    # Auto-detect current session (env vars, tmux fingerprinting)
├── transcript.rs        # Common transcript types (Role, EntryType, TranscriptEntry)
├── activity.rs          # Activity event parsing (messages + permissions)
├── config.rs            # Config loading (~/.config/ai-audit/config.yml)
└── rate/                # Rate module (test parsing, judge invocation)
```

### Design Principles

- **Provider-agnostic core**: Common traits/types in `lib.rs` and `transcript.rs`
- **Provider modules**: Each AI assistant gets its own module (`claudecode/`, `opencode/`)
- **Action-based commands**: CLI structured around actions, not providers.
  Provider is auto-detected or specified via `-t` flag.

## CLI Commands

The CLI uses a `<noun> <verb>` structure for session-scoped commands.
All session inspectors plus per-session ops (`usage`, `assisted-by`)
live under `ai-audit session <verb>`. Cross-session concerns
(`activity`, `token-usage`, `rate`) stay at the top level.

```
ai-audit <command> [options]

# Top-level commands:
ai-audit session <verb> [options]
ai-audit activity list | get <timespan> [IDENT...]
ai-audit token-usage <timespan> [filters...]
ai-audit rate <instruction> --test <path>

# session subcommands (with aliases):
ai-audit session list         [aliases: ls]      [-s TEXT] [--timespan EXPR] [-p PATH] [-t TYPE]
ai-audit session current      [aliases: cur]     [--match TEXT | --pid PID] [-t TYPE]
ai-audit session previous     [aliases: prev]    [-t TYPE]
ai-audit session transcript   [aliases: tr]      [SESSION-ID] [-n LAST]
ai-audit session permissions  [aliases: perms]   <session-id>
ai-audit session usage        [aliases: tokens]  [SESSION-ID] [filters...]
ai-audit session assisted-by                     [--session ID]
ai-audit session info                            [SESSION-ID]
ai-audit session nudge                           <session-id | --all>
```

Short forms work via clap's `infer_subcommands` plus explicit
aliases: `ai-audit s ls`, `ai-audit s cur`, `ai-audit s pr`
(previous), `ai-audit s pe` (permissions). The prefix `s p` is
**ambiguous** between `permissions` and `previous` and is rejected
with a clear error — use `pe` / `pr` to disambiguate.

### Legacy top-level commands (deprecated, hidden)

The old top-level forms still parse for backward compatibility but
emit a one-line deprecation warning on stderr:

| Old (still works)          | New (canonical)                |
|----------------------------|--------------------------------|
| `ai-audit list-sessions`   | `ai-audit session list`        |
| `ai-audit current-session` | `ai-audit session current`     |
| `ai-audit last-session`    | `ai-audit session previous`    |
| `ai-audit transcript`      | `ai-audit session transcript`  |
| `ai-audit permissions`     | `ai-audit session permissions` |
| `ai-audit usage`           | `ai-audit session usage`       |
| `ai-audit assisted-by`     | `ai-audit session assisted-by` |

Note the rename: `last-session` is now `session previous` (the verb
form is friendlier and avoids any `l` ambiguity with `list`).

## Data Sources

### Claude Code
- Session transcripts: `~/.claude/projects/<encoded-path>/<uuid>.jsonl`
- Debug logs: `~/.claude/debug/<uuid>.txt`
- Settings: `~/.claude/settings.json`

### OpenCode
- **Primary store**: `~/.local/share/opencode/opencode.db` — SQLite (WAL,
  Drizzle migrations). Owned by the upstream opencode binary
  (`~/dev/ts/opencode/packages/opencode/src/storage/db.ts`). Channel
  suffix may apply: non-`latest`/`beta`/`prod` channels use
  `opencode-<channel>.db` instead, and the `OPENCODE_DB` flag can
  redirect to a custom file. ai-audit currently hard-codes
  `opencode.db` (see `src/opencode/db.rs::db_path()` — does NOT honor
  channel suffix or `OPENCODE_DB`).
- **Tables** (see `migration/*/migration.sql` upstream):
  - `session` — id, project_id, parent_id, slug, directory, title,
    version, share_url, summary_*, `revert` (JSON, set when the
    session was reverted to a prior cutoff), `permission`, `time_*`.
  - `message` — id, session_id, time_created, time_updated, `data`
    (JSON; opencode's full MessageV2 blob including `$.role`,
    `$.time.completed`, `$.error`, `$.agent`, `$.model`).
  - `part` — id, message_id, session_id, time_created, time_updated,
    `data` (JSON; per-part shape: text / tool / step-start / file /
    etc., with tool parts carrying `$.state.status` ∈
    {pending, running, completed, error} and
    `$.state.metadata.interrupted` when aborted).
  - `permission` — per-project (PK is `project_id`), NOT per-session.
  - `todo` — session todos.
  - `session_share`, `session_entry`, `project`.
- **Status detection contract** (documented by upstream at
  `src/session/session.ts:155-175` for `Info`): the *static* status of
  a session is derived from the LAST message + ALL its parts:
  - `lastMessageRole`, `lastMessageTimeCreated`,
    `lastMessageTimeCompleted`, `lastMessageErrored` (`$.error IS NOT
    NULL`), `partsTotal`, `stuckTools` (count of tool parts with
    `state.status IN ('running','pending')` OR `state.status='error'
    AND state.metadata.interrupted=1`).
  - ai-audit's mirror lives in `src/opencode/status.rs` (`StaticStatus`
    enum, `fetch_last_message_meta`, `classify_static`).
- **Legacy file-tree** (still referenced by older code paths —
  `src/opencode/transcript.rs`, `src/activity.rs`,
  `src/opencode/session_index.rs`, `src/opencode/mod.rs`):
  `~/.local/share/opencode/storage/{session,message,part}/...`.
  Upstream opencode now writes EXCLUSIVELY to the SQLite DB; these
  directories are leftovers from a pre-migration deployment and
  should NOT be treated as the source of truth. Any new feature MUST
  read from `opencode.db` via `src/opencode/db.rs`. The legacy
  read-paths are kept only to surface historical data on machines
  that still have those files; they will be removed once the
  migration is universally rolled out.
- **Logs**: `~/.local/share/opencode/log/*.log`.
- **Live status**: queried from the running opencode HTTP server via
  `GET /session/status` (see `src/opencode/server_client.rs`).
  Returns Running/Idle/ServerUnreachable. "Idle" only means *this
  session is not currently being processed by the live server* — it
  is orthogonal to the static status. A session can be
  `static=assistant-empty` (the assistant turn died with no parts
  produced) AND `live=idle` (no live work in flight). That combo is
  the "interrupted with nothing to show for it" shape; the static
  status alone tells you whether the turn was completed cleanly or
  errored mid-stream (`$.error IS NOT NULL` on the last message).

### Pi (badlogic/pi-mono)
- Sessions: `~/.pi/agent/sessions/--<encoded-cwd>--/<iso-ts>_<uuidv7>.jsonl`
- Sub-agent sessions (e.g. spawned by `pi-subagents`):
  `~/.pi/agent/sessions/--<encoded-cwd>--/<iso-ts>_<parent-uuid>/<entry-id>/run-N/session.jsonl`
- Settings: `~/.pi/agent/settings.json`
- Base dir override: `PI_CODING_AGENT_DIR` environment variable.
- **Authoritative `cwd`**: read from the JSONL header line.  NEVER
  decode the `--<encoded-cwd>--` directory name (the encoding `/` → `-`
  is lossy and ambiguous).
- Pi has **no permission/approval model**, so the `permissions` command
  errors out for pi sessions and only `pi-msg@<project>` activity
  identifiers are emitted.
- `PI_SESSION_ID` is exported into the agent environment by the
  separate `pi-env-session-id` pi extension, so child processes spawned
  by pi's `bash` tool inherit it.

## Session Detection Rules

**BLOCKING**: These rules have no exceptions.

- **NEVER** use `/proc/<pid>/cmdline` or any command-line parsing to
  extract session IDs. The session can change (e.g. via Ctrl+P in
  OpenCode) without the cmdline changing, making this fundamentally
  unreliable.
- **NEVER** use the current working directory (CWD) to guess which
  session is active. CWD can change independently of the session.
- **Only two valid detection methods**:
  1. **Environment variables** (`OPENCODE_SESSION_ID`,
     `CLAUDE_SESSION_ID`, `PI_SESSION_ID`) — authoritative when set.
     `PI_SESSION_ID` requires the companion `pi-env-session-id` pi
     extension; pi itself does not export the session ID.
  2. **Tmux scrollback fingerprinting** — parse TUI ANSI rendering
     into structured filters, then match against session databases.
     For pi, only depth-0 `TextContains` matching is used (pi's TUI
     icons differ from OpenCode's, so structured tool-field filters
     do not apply).
- If neither method yields a result, return an error (exit code 1).
  Never fall back to heuristics.

## Development Notes

- Session ID format (auto-detected via UUID version nibble at index 14):
  - `ses_*` → OpenCode
  - UUIDv4 (version `4`) → Claude Code
  - UUIDv7 (version `7`) → pi
  - Anything else → loud error (no silent fallback).
- Config: `~/.config/ai-audit/config.yml` (path simplification rules)
- Tests: `cargo test` (300+ unit tests)
- Build: `cargo build --release`
