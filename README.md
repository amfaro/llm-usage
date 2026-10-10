# llm-usage

Standalone terminal dashboard for subscription quota windows. Supports Codex, OpenCode Go, and Claude Code without importing Pi.

```sh
cargo run -- watch                 # refresh every 30 seconds
cargo run -- once                  # one terminal snapshot
cargo run -- json                  # one stable JSON snapshot
cargo run -- json --interval 60 --output ~/.cache/llm-usage/usage.json
cargo run -- once --provider codex
cargo run -- watch --interval 60 --no-color
```

`watch` is the default command. Press `q` to exit. Use `--provider codex`, `--provider opencode-go`, or `--provider claude-code` repeatedly to filter providers.

### Status bars and widgets

Widgets that poll on a timer should read a file kept fresh by one long-running process rather than spawn `json` on every refresh:

```sh
llm-usage json --source cliproxy --interval 60 --output ~/.cache/llm-usage/usage.json
```

`--output` atomically replaces the file (temporary file plus rename), so readers never see a partial write. Its contents match what `json` prints. `--interval` requires `--output` and keeps the process running until it is terminated, so run it under a service manager such as launchd or systemd. Because one process serves every refresh, in-memory Claude throttling, `Retry-After` cooldowns, discovery caching, and bounded stale fallback all apply between refreshes. Provider failures appear in the JSON as usual; a failed file write is reported on stderr and retried on the next refresh. `--output` without `--interval` writes one snapshot and exits.

## CLIProxyAPI proof of concept

### Quick start with Doppler

From the PoC worktree, in fish or any other shell:

```sh
mise run proxy        # one live snapshot: Codex, OpenCode Go, and Claude Code
mise run proxy watch  # continuous dashboard
mise run proxy json   # JSON output
```

This task injects only `WORK_LLM_USAGE_CLIPROXY_URL` and
`WORK_LLM_USAGE_CLIPROXY_MANAGEMENT_KEY` from Doppler project `shared`, config
`dev_personal`. It disables Doppler's disk fallback cache and keeps your existing
`PI_CODING_AGENT_DIR` and `CLAUDE_CONFIG_DIR`. A pre-existing Doppler CLI login is
required. All proxy settings accept their `WORK_`-prefixed equivalent; a nonempty
unprefixed setting takes precedence.

### Manual configuration

Use `--source cliproxy` on `watch`, `once`, or `json` to fetch **subscription
quota windows** using credentials held by a remote CLIProxyAPI server. Local
credentials are the default (`--source local`); setting proxy environment
variables alone does not change the source.

This PoC targets the **v8 management API**, as served by CLIProxyAPI 8.0.23.
It does not use the older `/v0/management` API.

```sh
export LLM_USAGE_CLIPROXY_URL='http://100.97.112.40:8318'
# Supply this through your work secret manager or a hidden shell prompt.
# In Bash, this avoids putting the secret into shell history:
read -rsp 'CLIProxyAPI management key: ' LLM_USAGE_CLIPROXY_MANAGEMENT_KEY
printf '\n'
export LLM_USAGE_CLIPROXY_MANAGEMENT_KEY

cargo run -- json --source cliproxy --provider codex
cargo run -- once --source cliproxy --provider claude-code
cargo run -- once --source cliproxy --provider opencode-go
cargo run -- watch --source cliproxy  # all three providers

# Remove the key from this shell when finished:
unset LLM_USAGE_CLIPROXY_MANAGEMENT_KEY
```

Run these commands from the PoC checkout, not a checkout without this feature.
When using mise-managed tools noninteractively, prefix commands with `mise exec --`.
`LLM_USAGE_CLIPROXY_URL` accepts an HTTP(S) server base URL or a URL ending in
`/v8/management`; do not supply the `management.html#/auth-files` page URL.

### Remote credentials and account selection

The management key is the **management UI login key**, not a normal inference
API key. It grants administrative access, so use a trusted server and HTTPS or
an encrypted private network. The program never logs the key, does not accept
it as a command-line argument, and refuses management HTTP redirects.

The client lists remote credential metadata using
`GET /v8/management/credentials`, then submits an upstream quota `GET` through
`POST /v8/management/requests/api-call`. Its upstream Authorization header is
literally `Bearer $TOKEN$`; CLIProxyAPI substitutes the selected credential
server-side. The PoC does not download auth files, request credential refreshes,
or write provider tokens anywhere. Provider refresh is left to the proxy.

A single enabled matching credential is selected automatically. If multiple
accounts match, the provider stays unavailable until you select one explicitly:

```sh
export LLM_USAGE_CLIPROXY_CODEX_AUTH_INDEX='<Codex auth_index>'
export LLM_USAGE_CLIPROXY_CLAUDE_AUTH_INDEX='<Claude auth_index>'
export LLM_USAGE_CLIPROXY_OPENCODE_GO_AUTH_INDEX='<Go auth_index>'
```

Use the `auth_index` from remote metadata, not the filename or API key. Listed
disabled accounts and accounts of a different provider are never selected.
Configured Go API-key groups may be absent from this metadata; the client can
instead discover their indexes from scoped provider configuration as described
below.
For Codex, account IDs are taken from credential metadata/attributes or a JSON
`id_token`. If the server only provides a JWT string, or you need a specific
account ID, supply it explicitly:

```sh
export LLM_USAGE_CLIPROXY_CODEX_ACCOUNT_ID='<ChatGPT account ID>'
```

### OpenCode Go through CLIProxyAPI

Use the Go API key already configured **on the proxy server**, in an
OpenAI-compatible provider (group) with upstream base URL
`https://opencode.ai/zen/go/v1`. No additional provider or local key is needed
when that group is already configured. For new setup, use the server's
configuration or management UI and your approved secret manager; do not copy
personal credentials into a work profile.

**Important v8.0.23 distinction:** config-synthesized compatible API keys can be
omitted from `GET /v8/management/credentials` because they have neither a backing
auth file nor the `runtime_only` attribute. Their absence does **not** mean the
provider is unconfigured. `POST /v8/management/requests/api-call` can still resolve
their runtime `auth_index`.

**Automatic discovery is the default; no manual index is needed for a single
matching enabled key.** If ordinary credential metadata does not expose the Go
group, the client reads only
`GET /v8/management/config/api-keys/openai-compatibility`. It matches the configured
group name (default `opencode-go`), checks that the upstream URL is HTTPS on
`opencode.ai` at `/zen/go/v1`, and selects its per-key `auth_index`. Unrelated or
disabled groups/keys are not selected. Missing, malformed, duplicate, or conflicting
indexes fail closed rather than silently dropping entries to choose another key.

**Security trade-off:** the scoped configuration response includes raw API keys,
headers, and other fields, potentially for other compatible groups too. Those
bytes briefly enter client memory. Typed deserialization discards secret fields;
only the selected group's name, canonical upstream URL, enabled state, and opaque
index are retained. Raw keys are never logged, persisted, cached, or forwarded.
Use this only with an approved work-provider configuration. The client does not
fetch the complete server configuration or auth files, and never uses config
metadata to override a listed unrelated or disabled credential.

Sanitized discovery metadata is cached **in memory only** for five minutes within
a running session; no raw configuration response is cached. Failed discovery
backs off for at least five minutes (or the server's `Retry-After` for HTTP 429).
Key rotation is picked up on the next metadata refresh, or immediately after
restarting `watch`. Config-discovery failures leave Go unavailable without
preventing Codex or Claude from reporting usage.

When compatible-key metadata is exposed by `/credentials`, it is used directly
without reading configuration. An exact `openai-compatible-opencode-go` provider
(or `opencode-go`) is recognized; generic `openai-compatibility` metadata requires
an exact group label. Unrelated compatible groups and OAuth accounts are never
guessed to be Go credentials.

If your configured Go group uses another name, identify it explicitly:

```sh
export LLM_USAGE_CLIPROXY_OPENCODE_GO_PROVIDER='company-go'
```

The group setting also accepts its internal provider key
(`openai-compatible-company-go`). For multiple matching keys, select one with
`LLM_USAGE_CLIPROXY_OPENCODE_GO_AUTH_INDEX='<Go auth_index>'`. An explicit hidden
16-character hexadecimal index is an operator-approved Go reference and avoids
configuration reads; the client cannot verify its group if metadata is hidden.
Listed indexes still must pass the normal exact-group and disabled checks. This
optional override is not needed for normal single-key discovery.

Both settings accept their `WORK_` variants. The Doppler task injects only the two
connection settings listed above; provide optional selectors/group settings in
the invoking environment.

The dashboard submits `GET https://opencode.ai/zen/go/v1/usage` through
`POST /v8/management/requests/api-call`, with `Bearer $TOKEN$`. CLIProxyAPI replaces
the placeholder with the selected server-held API key. The existing Go parser
reports the rolling 5h, weekly 7d, and monthly 30d windows with
`source: "cliproxy"` and plan `Go`.

Missing, disabled, or ambiguous credentials leave Go unavailable with a redacted
diagnostic; other configured providers continue working. There is **no local
fallback**, even if `OPENCODE_GO_API_KEY` is set. A Go subscription is required;
upstream rejection, malformed usage, and rate limits remain unavailable.

### Behavior and limitations

- **No local fallback:** proxy mode never calls local provider credential
  discovery, even if remote configuration, authentication, or quota checks fail.
  It neither reads `$PI_CODING_AGENT_DIR/auth.json` nor Claude's local credentials
  or local quota cache. Keep both work-profile variables (`PI_CODING_AGENT_DIR`
  and `CLAUDE_CONFIG_DIR`) set when working in this repository.
- Codex, OpenCode Go, and Claude use the same upstream quota endpoints and window
  parsers as local mode. Remote output has `source: "cliproxy"`; the existing JSON
  schema, derived quota state, and presentation fields are unchanged.
- The proxy task selects all three providers. Use `--provider` with manual commands
  to restrict the selection; providers without remote credentials remain visible
  as unavailable.
- This reports actual upstream subscription quotas, **not proxy request/token
  counters**. The observability usage API is not integrated in this PoC.
- Claude quota requests are limited to one successful refresh per five minutes
  within a running session. HTTP 429 honors upstream `Retry-After` (minimum one
  minute; default 15 minutes). Transient quota/discovery failures retain sanitized
  cached windows as stale for at most one hour; authentication rejection discards
  cached usage. Discovery failures also pause management requests for at least
  five minutes, or for `Retry-After` when the management API returns HTTP 429.
  The remote cache is **in memory only**, isolated by server session and exposed
  account identity/revision metadata. If a credential is replaced without any
  identity or revision metadata changing, restart `watch` to discard old state.
  Restarting `watch`, or repeatedly invoking `once`/`json`, does not share cooldowns
  or cached data. Prefer a long-running `watch`, or `json --interval` for widgets,
  rather than rapid repeated polls.
- Management-key rejection and upstream credential rejection produce distinct,
  redacted diagnostics; upstream response bodies are never printed.
- Automated mock-HTTP tests cover all three providers, credential selection,
  token placeholders, and failure cases. Live Codex, OpenCode Go, and Claude quotas
  were verified using the Doppler-backed configuration, including automatic Go
  discovery from the existing server group. Go discovery tests cover the
  scoped configuration read, secret stripping, metadata caching, key rotation,
  upstream validation, ambiguity, and optional explicit indexes.

## Credentials (local mode)

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

`once` and `json` exit `0` when at least one selected provider returns usage, otherwise `1`. `json --output` also exits `1` when the file cannot be written. Unavailable providers remain in text and JSON output with a redacted error. `watch` and `json --interval` keep retrying on later refreshes.

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
- `capacity_used_percent`: integer 0–100, the limiting window's `used_percent`,
  rounded (selection rule below). It is `100` whenever provider
  `status` is `rate_limited`, even if no window reached 100%. Omitted when the
  provider is `unavailable` or has no window with a usage value.
- `windows`: every window that is not `unavailable` and reports usage, in
  response order, with the rounded integer `used_percent`; the raw values stay
  in the top-level `windows`. Omitted when empty.
- `limiting_window`: a `rate_limited` window first, otherwise the shortest
  usable window (not necessarily the highest `used_percent`). Ties keep
  response order. `used_percent` here is the
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
- `freshness`: update time plus the `↻` and `≈` legend. The clock is `fetched_at`
  rendered in the machine's local timezone, so it is not a stable value.

A `label` is the provider name padded to a fixed column, then its status. The
status lists every usable window, while `summary` keeps only the limiting one:

| Provider state | Status text |
| --- | --- |
| usable | `42% 5h ↻2h · 12% 7d ↻3d` — usage, window, reset in, per window |
| `stale` | same, plus ` ≈` |
| `rate_limited` | same, with each window's real usage; check `exhausted` |
| no comparable capacity | `no usage data` |
| `unavailable` | `unavailable` |

`↻` durations use the largest whole unit (`7d`, `3h`, `12m`, `30s`, `now`).
`visible` is `true` when a provider reports usable capacity, so compact
widgets can drop noise while fuller UIs still render every entry.

`schema_version` changes only for breaking field removals, type changes, or semantic changes. New optional fields may be added without changing it. Version 2 adds the `stale` provider status so cached usage is distinct from unavailable usage. It retains `available` and `limit_reached` for existing consumers; new consumers should use `status` instead. `display`, `best_available`, and `presentation` are additive derivations of those fields, so they do not change the version.
