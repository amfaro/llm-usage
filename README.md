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
  "schema_version": 2,
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
      "fetched_at": 1783871200,
      "display": {
        "name": "Codex",
        "exhausted": false,
        "capacity_used_percent": 42,
        "windows": [
          {
            "label": "5h",
            "used_percent": 42,
            "reset_at": 1783874800
          }
        ],
        "limiting_window": {
          "label": "5h",
          "used_percent": 42,
          "reset_at": 1783874800
        },
        "next_reset_at": 1783874800
      }
    }
  ],
  "best_available": {
    "provider": "codex",
    "capacity_used_percent": 42
  },
  "presentation": {
    "summary": "Codex 42% 5h ↻2h",
    "severity": "ok",
    "providers": [
      {
        "provider": "codex",
        "label": "Codex       42% 5h ↻2h",
        "visible": true
      }
    ],
    "freshness": "Updated 14:03:22 · ↻ = reset in"
  }
}
```

Provider and window `status` values are:

- `ok`: usage was fetched and the quota is usable.
- `stale`: cached provider usage remains usable, but its latest refresh failed or was throttled.
- `rate_limited`: a window reached 100% or the provider marked it rate-limited; provider status follows when any window is rate-limited.
- `unavailable`: provider usage could not be fetched, or an expected window was absent from the response.

`used_percent` remains numeric. `reset_at` and `fetched_at` are Unix timestamps in seconds, and `window_seconds` is a numeric duration in seconds. Optional fields are omitted when unavailable. Provider errors remain redacted and contain no credentials or upstream response bodies.

### Derived quota state

`display` and `best_available` are derived from the raw fields above so status
UIs do not reimplement quota semantics. They add no new information and never
change the meaning of a raw field.

`presentation.summary` uses compact provider names (`Codex`, `Go`, `Claude`),
while `presentation.providers[*].label` keeps the full provider names shown
below.

Per-provider `display`:

- `name`: human-readable provider label (`Codex`, `OpenCode Go`, `Claude Code`).
  Unknown provider IDs fall back to the ID itself.
- `exhausted`: `true` only when provider `status` is `rate_limited`. An
  `unavailable` provider is not exhausted; check `status` for that case.
- `capacity_used_percent`: integer 0–100, the highest `used_percent` among
  windows that are not `unavailable`, rounded. It is `100` whenever provider
  `status` is `rate_limited`, even if no window reached 100%. Omitted when the
  provider is `unavailable` or has no window with a usage value.
- `windows`: every window that is not `unavailable` and reports usage, in
  response order, with the rounded integer `used_percent`; the raw values stay
  in the top-level `windows`. Omitted when empty.
- `limiting_window`: the window that constrains the provider — a
  `rate_limited` window first, otherwise the highest `used_percent`. Ties keep
  response order, so the shortest window wins. `used_percent` here is the
  rounded integer; the raw value stays in `windows`. Omitted when no window
  reports usage.
- `next_reset_at`: soonest `reset_at` among windows that are not
  `unavailable`. Omitted when no such window has a reset.

Top-level `best_available` is the provider whose `status` is not `unavailable`
with the lowest `capacity_used_percent`, with snapshot order breaking ties. A
`stale` provider stays eligible because cached usage is still usable. A
provider with no comparable capacity (no windows, or only `unavailable`
windows) is skipped. It is `null` when no provider qualifies, and it may point
at a `rate_limited` provider at `100` when that is the only one left, so check
`exhausted` before treating it as usable.

### Presentation

`presentation` is compact human-facing status text for any widget or
dashboard. It is UI-agnostic: no colors, no markup, no shell, and it never
invokes a UI. Raw and derived values stay available alongside it, so a
consumer that wants its own wording can ignore this block entirely.

- `summary`: every visible provider on one line, joined with ` · `. It is
  `no usage data` when nothing is visible.
- `severity`: semantic only — `ok`, `warning`, `critical`, or `unknown`.
  Consumers choose colors. It describes `best_available`: `unknown` with no
  best provider, `critical` at 90% or more, `warning` at 70% or more,
  otherwise `ok`. These are the same thresholds the terminal dashboard
  colors use. A single exhausted provider does not make the snapshot
  `critical` while another provider still has room.
- `providers[]`: one entry per provider in snapshot order, with the stable
  `provider` ID, a `label`, and `visible`.
- `freshness`: update time plus the `↻` legend. The clock is `fetched_at`
  rendered in the machine's local timezone, so it is not a stable value.

A `label` is the provider name padded to a fixed column, then its status. The
status lists every usable window, while `summary` keeps only the limiting one:

| Provider state | Status text |
| --- | --- |
| usable | `42% 5h ↻2h · 12% 7d ↻3d` — usage, window, reset in, per window |
| `stale` | same, plus ` (stale)` |
| `rate_limited` | same, with each window's real usage; check `exhausted` |
| no comparable capacity | `no usage data` |
| `unavailable` | `unavailable` |

`↻` durations use the largest whole unit (`7d`, `3h`, `12m`, `30s`, `now`).
`visible` is `true` when a provider reports usable capacity, so compact
widgets can drop noise while fuller UIs still render every entry.

`schema_version` changes only for breaking field removals, type changes, or semantic changes. New optional fields may be added without changing it. Version 2 adds the `stale` provider status so cached usage is distinct from unavailable usage. It retains `available` and `limit_reached` for existing consumers; new consumers should use `status` instead. `display`, `best_available`, and `presentation` are additive derivations of those fields, so they do not change the version.
