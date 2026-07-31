# llm-usage

Standalone terminal dashboard for subscription quota windows. Supports Codex and OpenCode Go without importing Pi.

```sh
cargo run -- watch                 # refresh every 30 seconds
cargo run -- once                  # one terminal snapshot
cargo run -- json                  # one stable JSON snapshot
cargo run -- once --provider codex
cargo run -- watch --interval 60 --no-color
```

`watch` is the default command. Press `q` to exit. Use `--provider codex` or `--provider opencode-go` repeatedly to filter providers.

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

### OpenCode Go

OpenCode does not currently provide a public Go plan quota API ([issue #16017](https://github.com/anomalyco/opencode/issues/16017)). The tool therefore reads the authenticated Go dashboard page. Set both values explicitly:

```sh
export OPENCODE_GO_WORKSPACE_ID='your-workspace-id'
export OPENCODE_GO_AUTH_COOKIE='your-auth-cookie-value'
```

`OPENCODE_GO_AUTH_COOKIE` may be either the cookie value or `auth=<value>`. The tool does not extract browser cookies.

## Exit status

`once` and `json` exit `0` when at least one selected provider returns usage, otherwise `1`. Unavailable providers remain in text and JSON output with a redacted error. `watch` keeps retrying on later refreshes.

## JSON shape

```json
{
  "fetched_at": 1783871200,
  "providers": [
    {
      "provider": "codex",
      "available": true,
      "plan": "plus",
      "source": "oauth",
      "windows": [
        { "label": "5h", "used_percent": 42, "reset_at": 1783874800, "window_seconds": 18000 }
      ],
      "fetched_at": 1783871200
    }
  ]
}
```
