# ai-audit Project Guidelines

## Purpose

CLI tool to audit and monitor AI assistant sessions. Supports Claude
Code and OpenCode.

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
├── session_detect.rs    # Auto-detect current session (env, process tree, match, PID)
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

```
ai-audit <action> [options]

# Commands:
ai-audit list-sessions [-s TEXT] [--timespan EXPR] [-p PATH] [-t TYPE]
ai-audit current-session [--match TEXT | --pid PID] [-t TYPE]
ai-audit transcript [SESSION-ID] [-n LAST]
ai-audit permissions <session-id>
ai-audit activity list | get <timespan> [IDENT...]
ai-audit rate <instruction> --test <path>
```

## Data Sources

### Claude Code
- Session transcripts: `~/.claude/projects/<encoded-path>/<uuid>.jsonl`
- Debug logs: `~/.claude/debug/<uuid>.txt`
- Settings: `~/.claude/settings.json`

### OpenCode
- Sessions: `~/.local/share/opencode/storage/session/<hash>/ses_*.json`
- Messages: `~/.local/share/opencode/storage/message/<session-id>/msg_*.json`
- Parts: `~/.local/share/opencode/storage/part/<msg-id>/prt_*.json`
- Logs: `~/.local/share/opencode/log/*.log`

## Development Notes

- Session ID format: `ses_*` = OpenCode, UUID = Claude Code (auto-detected)
- Config: `~/.config/ai-audit/config.yml` (path simplification rules)
- Tests: `cargo test` (160+ unit tests)
- Build: `cargo build --release`
