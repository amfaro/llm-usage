# llm-usage

Standalone terminal dashboard for subscription quota windows. Supports Codex, OpenCode Go, and Claude Code without importing Pi.

```sh
cargo run -- watch                 # refresh every 30 seconds
cargo run -- once                  # one terminal snapshot
cargo run -- json                  # one stable JSON snapshot
cargo run -- once --provider codex
cargo run -- watch --interval 60 --no-color
```

`watch` is the default command. Press `q` to exit. Use `--provider codex`, `--provider opencode-go`, or `--provider claude-code` repeatedly to filter providers.

## Credentials

Secrets are read but never written or printed.

### Codex

Lookup order:

1. `LLM_USAGE_CODEX_ACCESS_TOKEN` and optional `LLM_USAGE_CODEX_ACCOUNT_ID`
2. `LLM_USAGE_CODEX_AUTH_FILE`
3. `$PI_CODING_AGENT_DIR/auth.json` when `PI_CODING_AGENT_DIR` is set
4. `~/.pi/auth.json`
5. `~/.codex/auth.json`

The discovered credential must be an OAuth credential. Log in again with Codex or Pi when it expires. When the upstream response omits a window (currently the Codex 5h window), the dashboard retains a muted `unavailable` row; it returns automatically when supplied again.

### Claude Code

Claude Code authentication mirrors the Codex lookup order, but uses a Claude Code OAuth access token from `~/.claude/.credentials.json` (or macOS Keychain):

1. `LLM_USAGE_CLAUDE_CODE_ACCESS_TOKEN`
2. `LLM_USAGE_CLAUDE_CODE_AUTH_FILE`
3. `~/.claude/.credentials.json`
4. macOS Keychain entries whose service starts with
   `Claude Code-credentials` (enumerated via `security`; the freshest
   unexpired token wins).

The token is sent as a `Bearer` token to Anthropic's internal OAuth usage endpoint. Rejected credentials (for example, an `ANTHROPIC_API_KEY` instead of an OAuth session) remain unavailable with a clear message.

> [!NOTE]
> The Claude Code usage endpoint is undocumented and may change or rate-limit aggressively. Successful usage is cached without credentials or response bodies under `$XDG_CACHE_HOME/llm-usage` (or `~/.cache/llm-usage`) and refreshed at most every five minutes. Rate limits pause requests for `Retry-After` or 15 minutes by default. Retryable failures keep cached windows visible with a stale age for at most 60 minutes; authentication failures never use stale data.

### OpenCode Go

```sh
export OPENCODE_GO_API_KEY='your-opencode-go-api-key'
```

The API key is sent as a `Bearer` token to `GET https://opencode.ai/zen/go/v1/usage`.

## Exit status

`once` and `json` exit `0` when at least one selected provider returns usage, otherwise `1`. Unavailable providers remain in text and JSON output with a redacted error. `watch` keeps retrying on later refreshes.

## JSON contract

```json
{
  "schema_version": 1,
  "fetched_at": 1783871200,
  "providers": [
    {
      "provider": "codex",
      "status": "ok",
      "available": true,
      "plan": "plus",
      "source": "oauth",
      "windows": [
        {
          "label": "5h",
          "status": "ok",
          "used_percent": 42,
          "reset_at": 1783874800,
          "window_seconds": 18000
        }
      ],
      "fetched_at": 1783871200
    }
  ]
}
```

Provider and window `status` values are:

- `ok`: usage was fetched and the quota is usable.
- `rate_limited`: a window reached 100% or the provider marked it rate-limited; provider status follows when any window is rate-limited.
- `unavailable`: provider usage could not be fetched, or an expected window was absent from the response.

`used_percent` remains numeric. `reset_at` and `fetched_at` are Unix timestamps in seconds, and `window_seconds` is a numeric duration in seconds. Optional fields are omitted when unavailable. Provider errors remain redacted and contain no credentials or upstream response bodies.

`schema_version` changes only for breaking field removals, type changes, or semantic changes. New optional fields may be added without changing it. Version 1 retains `available` and `limit_reached` for existing consumers; new consumers should use `status` instead.
