# Resurrect-Fix: Debug and fix `ai-audit last-session` failures

Use when `ai-audit last-session` fails to detect sessions from tmux
scrollback fingerprinting, preventing OpenCode session resurrection.

## Identifying failing panes

Find all tmux panes where an OpenCode session is visible but
`ai-audit last-session` fails to detect it:

```bash
# Iterate over all panes, test each one
for pane in $(tmux list-panes -a -F '#{session_name}:#{window_index}.#{pane_index}'); do
  # Check if pane shows an OpenCode footer (╹▀▀▀)
  if tmux capture-pane -t "$pane" -p | grep -q '╹▀▀'; then
    # Try detection using saved scrollback
    scrollback=$(tmux capture-pane -t "$pane" -p -S -200)
    result=$(echo "$scrollback" | ai-audit last-session --scrollback-file /dev/stdin 2>/dev/null)
    if [ -z "$result" ]; then
      echo "FAIL: $pane"
    fi
  fi
done
```

## Evidence collection

For each failing pane, create an evidence folder and save the
scrollback (200 lines is sufficient):

```bash
# Create evidence folder named by symbolic tmux address
mkdir -p "doc/pane-bug/<session>:<window>.<pane>"

# Save raw scrollback (200 lines is enough)
tmux capture-pane -t "<session>:<window>.<pane>" -p -S -200 \
  > "doc/pane-bug/<session>:<window>.<pane>/scrollback.dat"

# Generate verbose trace log
ai-audit -vvv last-session \
  --scrollback-file "doc/pane-bug/<name>/scrollback.dat" \
  2> "doc/pane-bug/<name>/run-fails.log"
```

Run the analysis script to produce structured summaries:

```bash
python3 doc/pane-bug/extract-filters.py --save
```

This generates `analysis.txt` per folder showing: filters built,
sessions searched, partial matches, hit rates, and final result.

## Iterative fix loop

Repeat until all folders are resolved or triaged:

### 1. Pick the next evidence folder

Read its `analysis.txt`. Present the filters, partial matches, and
failure point.

### 2. Diagnose the root cause

Compare filter criteria against what the DB actually stores. Common
failure patterns:

- **Part type mismatch**: filter text exists in a `tool` part but
  search only checks `text` parts.
- **TUI truncation**: filter value is truncated by terminal width;
  exact match fails against the full stored value.
- **TUI chrome in filter**: filter includes rendering artifacts not
  in the DB (`$ ` command prefix, ANSI/OSC escapes, panel borders).
- **Non-session content**: filter built from prompt input area,
  session title, or other TUI metadata that is never stored as
  session messages.
- **Window sizing**: correct session found but deeper filters
  reference messages beyond the search window.
- **Stale data**: session was deleted or compacted since the
  scrollback was captured.

### 3. Document the bug

Add a new heading in `doc/pain-bug.org` with:
- Description of the failure mechanism
- Evidence (which folder, which filter, what the trace shows)
- Proposed fix

### 4. Implement and test

```bash
# Edit the relevant source file
# Add a test for the fix
cargo test
```

### 5. Rebuild

```bash
cargo build --release
```

### 6. Re-run all remaining folders

```bash
for d in <remaining folders...>; do
  printf "%-20s " "$d"
  ai-audit -vvv last-session \
    --scrollback-file "doc/pane-bug/$d/scrollback.dat" \
    2> "doc/pane-bug/$d/run-fails.log" \
    && echo "OK" || echo "FAIL"
done
```

### 7. Regenerate analysis

```bash
python3 doc/pane-bug/extract-filters.py --save
```

### 8. Handle resolved folders

For each folder that now succeeds:

1. **Check the tmux pane** using its symbolic name:
   ```bash
   tmux capture-pane -t "<name>" -p | tail -15
   ```

2. **If the pane has a free prompt** and the resurrection command is
   visible in scrollback history, send it:
   ```bash
   tmux send-keys -t "<name>" \
     'LAST_SESSION=$(ai-audit last-session) && oc -s "$LAST_SESSION"' Enter
   ```

3. **Wait ~15 seconds**, verify the OpenCode TUI loaded:
   ```bash
   tmux capture-pane -t "<name>" -p | tail -3
   ```

4. **Delete the evidence folder**:
   ```bash
   rm -rf "doc/pane-bug/<name>"
   ```

### 9. Skip unresolved folders

If the folder still fails after the current fix, leave it for the
next iteration.

### 10. Loop

Return to step 1 with the remaining folders. Continue until all are
resolved or categorized as unfixable (stale data, window sizing).

## Key files

| File | Purpose |
|------|---------|
| `src/session_detect.rs` | Scrollback parser, filter building, TUI line classifier |
| `src/opencode/db.rs` | Filter matching against DB (`TextContains`, `ToolFieldEquals`) |
| `src/opencode/mod.rs` | `part_contains_needle` — searches all part types |
| `doc/pane-bug/extract-filters.py` | Parse `run-fails.log` → `analysis.txt` summaries |
| `doc/pain-bug.org` | Bug documentation with evidence and fix status |


