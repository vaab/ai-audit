# Claude Code Usage API

## Endpoint

```
GET https://api.anthropic.com/api/oauth/usage
```

## Authentication

Requires OAuth access token from Claude Code credentials:

```bash
curl -s \
  -H "Authorization: Bearer $(jq -r '.claudeAiOauth.accessToken' ~/.claude/.credentials.json)" \
  -H "anthropic-beta: oauth-2025-04-20" \
  "https://api.anthropic.com/api/oauth/usage"
```

## Response

Returns JSON with utilization data:

```json
{
  "five_hour": {
    "utilization": 0.15,
    "reset_at": "2026-01-26T12:00:00Z"
  },
  "seven_day": {
    "utilization": 0.42,
    "reset_at": "2026-02-02T00:00:00Z"
  }
}
```

### Fields

| Field | Description |
|-------|-------------|
| `five_hour.utilization` | Usage ratio (0.0-1.0) for rolling 5-hour window |
| `five_hour.reset_at` | When the 5-hour window resets |
| `seven_day.utilization` | Usage ratio (0.0-1.0) for rolling 7-day window |
| `seven_day.reset_at` | When the 7-day window resets |

## Notes

- The `anthropic-beta: oauth-2025-04-20` header is required
- Token location: `~/.claude/.credentials.json` (field: `claudeAiOauth.accessToken`)
- Utilization of 1.0 = rate limited
