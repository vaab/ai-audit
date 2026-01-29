# ai-audit Project Guidelines

## Purpose

CLI tool to audit and monitor AI assistant sessions. Currently supports
Claude Code, with room for other AI assistants (Codex, etc.).

## Architecture

### Modular Structure

```
src/
├── main.rs          # CLI entry point, command dispatch
├── lib.rs           # Common types and traits
├── claude/          # Claude Code specific
│   ├── mod.rs
│   ├── debug.rs     # Debug log parsing
│   ├── session.rs   # Session log parsing (future)
│   └── permissions.rs
├── codex/           # OpenAI Codex (future)
│   └── mod.rs
└── ...              # Other AI providers
```

### Design Principles

- **Provider-agnostic core**: Common traits/types in `lib.rs`
- **Provider modules**: Each AI assistant gets its own module (`claude/`, `codex/`, etc.)
- **Action-based commands**: Structure CLI around actions (permissions, tokens, sessions, etc.)
  not providers. Provider is either auto-detected or specified via flag.

### Planned Actions

Beyond permissions, consider:
- `tokens` - Token usage statistics per session
- `sessions` - List/search sessions
- `timeline` - Chronological view of session activity
- `costs` - Estimate API costs
- `tools` - Tool usage statistics
- `errors` - Error/failure analysis

## CLI Design

```
ai-audit <action> [options] <target>

# Examples:
ai-audit permissions <session-id>        # Claude (auto-detected)
ai-audit permissions --provider codex <session-id>
ai-audit tokens <session-id>
ai-audit sessions --list
ai-audit sessions --search "keyword"
```

## Development Notes

- Debug logs: `~/.claude/debug/<session-id>.txt`
- Session logs: `~/.claude/projects/<path>/<session-id>.jsonl`
- Settings: `~/.claude/settings.json`
