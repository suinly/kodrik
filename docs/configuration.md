# Configuration reference

Kodrik reads `~/.kodrik/config.toml`. Restart the process after changing it.
Settings are literal TOML values; environment-variable interpolation is not
implemented. Keep API keys and bot tokens out of the repository.

## Minimal configuration

```toml
[gateway]

[provider.primary]
type = "openai"
model = "your-model"
api_key = "your-key"
```

`provider.primary` and the `gateway` table are required. An empty gateway table
allows terminal use without Telegram.

## Providers

Both providers use the OpenAI Responses API adapter. A compatible endpoint must
support Responses, not only Chat Completions.

| Field | Required | Meaning |
| --- | --- | --- |
| `type` | Yes | `"openai"`, the supported adapter |
| `model` | Yes | Model identifier accepted by the endpoint |
| `api_key` | Yes | API credential |
| `api_base` | No | Custom API base URL; omitted uses the adapter's OpenAI default |

`primary` handles conversations, automatic learning and proactive research.
An optional `system_one` uses a separate, preferably fast model for decisions:

```toml
[provider.system_one]
type = "openai"
model = "your-fast-model"
api_key = "your-key"
# api_base = "https://your-provider.example/v1"
```

Without `system_one`, decisions use `primary` with separate limits. There is no
automatic model substitution after a provider error. See [System One](sessions.md).

## Telegram

Add a named gateway to the minimal configuration:

```toml
[gateway.telegram]
type = "telegram"
token = "your-bot-token"
```

The gateway accepts private messages only from the configured owner and users
admitted through a one-use invitation. Groups are disabled. Guest mode in private
chats is available to those same users, authenticated by the sender rather than
the other person's chat ID. Both directions of a private conversation share one
guest session, memory, datasets and virtual filesystem, identified by the sorted
pair of participant IDs. Other pairs and personal spaces remain separate.
Each model turn checks the current caller's access, even if the other participant
has lost access. Sender ID, name, username and quoted reply context reach the agent.
The name `telegram` is configurable; multiple named gateways are supported.
Run `kodrik gateway` to start polling. Create a separate link manually for each
person using `kodrik invites create --label "Alice"`; see
[owner-managed invitations](web-chat.md#owner-managed-invitations).
Each admitted user gets personal memory, skills, history, attachments and background
tasks, shared by private Telegram chats and web login. Only admitted users' saved
spaces and background tasks load on startup. CLI/TUI always uses the configured
owner. Other users use Bashkit's virtual filesystem.

To grant native shell access to your private chat, configure its positive user ID
and the matching gateway name:

```toml
[owner]
interface = "telegram"
user_id = 123456789
```

The gateway name must exist in the configuration. Without `[owner]`, all Telegram
users still require invitations and use Bashkit; CLI/TUI retain native access. Bashkit has no host
filesystem, inherited environment or network access, and never falls back to the
real shell. Wikipedia and optional web research remain separate tools. See [Storage](storage.md)
for paths and the one-time migration of existing owner data.

## MCP servers

Add named MCP servers using Streamable HTTP:

```toml
[mcp.rustrak]
type = "http"
url = "https://rustrak.suinly.com/mcp"

[mcp.rustrak.headers]
Authorization = "Bearer YOUR_TOKEN"
```

`type` and `url` are required; `headers` is optional and accepts HTTP header
names and literal string values. Add another `[mcp.<name>]` table for each server.
Restart Kodrik after changes. Environment-variable interpolation is not supported.
Keep tokens in the local configuration, outside the repository.

Kodrik initializes each configured server and discovers all pages of tools at
startup. A connection or discovery failure stops startup with the server name;
initialization and discovery share a 30-second deadline per server. Tool calls
have a 60-second deadline. Use the final endpoint URL: redirects are not followed.
URLs must use HTTP(S), without embedded credentials or a fragment.

Tools appear as `mcp__rustrak__<tool>`. The complete name must be at most 64 ASCII
letters, digits, underscores or hyphens; invalid or duplicate names fail startup.
The complete catalog stays in the application. The model initially receives
`mcp_search`, not all MCP schemas. It searches server/tool names and descriptions;
matching schemas become available on the next model request. Search defaults to
8 matches and accepts a limit of 1–16. Use an exact qualified name to find one
function. Queries use words from the original names/descriptions; English
keywords usually work best for English MCP catalogs.

Up to 32 recently discovered MCP tools remain advertised alongside built-ins,
within a total budget of 128 functions. Searching again reloads an older tool.
Selections are reconstructed from successful search results in saved history,
including after compaction and restart, and are not shared between conversations.
The complete catalog remains searchable. This uses ordinary function calls and
requires no native `tool_search` support from the provider.

The selected server input schemas are passed through with non-strict function calling.
Text, structured results and `isError` are preserved in tool results and history.
Headers are not included in tool definitions, search results or arguments.

Only the owner's foreground agent receives these tools (CLI/TUI and the owner's
personal chats). Isolated users, guest spaces and proactive research do not
connect to the configured servers. Calls use the server token's permissions,
including write operations. Calls are never marked safe for crash replay, and
HTTP failures or expired MCP sessions do not automatically repeat the request.
Local metadata searches are read-only and safe to repeat during recovery.
A timeout may leave the remote operation's outcome unknown. If the server expires
an MCP session, restart Kodrik to reconnect.

This version supports HTTP tools only. stdio, the legacy HTTP+SSE transport,
OAuth login flows, prompts, resources and interactive tool responses are not
implemented.

## Sessions

```toml
[sessions]
auto_switch = true
idle_minutes = 30
compact_threshold_tokens = 80000
compact_background_margin_tokens = 16000
restart_limit = 5
```

| Field | Default | Meaning |
| --- | --- | --- |
| `auto_switch` | `true` | Check whether an eligible message starts a new topic |
| `idle_minutes` | `30` | Minimum pause since the last user/assistant message; nonnegative integer |
| `restart_limit` | `5` | Consecutive crashed attempts in one semantic phase before parking; `0` disables automatic recovery |
| `compact_background_margin_tokens` | `16000` | Start preparing a summary this many tokens before the blocking threshold; `0` disables background only |
| `compact_threshold_tokens` | `80000` | Automatically summarize old context at this estimated input size; `0` disables, otherwise at least `4096` |

`idle_minutes = 0` checks every eligible turn. A pause only permits the check; it does not force
a new session. See [Sessions](sessions.md) for context, timeouts and fallbacks.

For a nonzero threshold, the background margin must be smaller than the threshold.
If an existing configuration uses a threshold of 16,000 or less, explicitly set
a smaller margin (or `0`); the default margin would fail validation.

Choose a compaction threshold below your model's context window, leaving room for
output and estimation error. It is not an automatically discovered model limit.
See [context compaction](sessions.md#automatic-context-compaction) for estimation
and failure handling.

## Companion

All values below are defaults; the section may be omitted:

```toml
[companion]
learning_enabled = true
proactive_enabled = true
timezone = "Europe/Moscow"
proactive_hour = 12
quiet_start = 22
quiet_end = 9
# Optional explicit recipient: set both fields together.
# interface = "telegram"
# chat_id = 123456789
```

| Field | Meaning |
| --- | --- |
| `learning_enabled` | Review successful substantive conversations for durable lessons |
| `proactive_enabled` | Allow at most one daily research/delivery attempt while the gateway runs |
| `timezone` | IANA timezone used for live agent clock context and the proactive schedule |
| `proactive_hour` | Earliest local hour for the daily attempt |
| `quiet_start`, `quiet_end` | Quiet-hour interval; hours must be 0–23 and differ |
| `interface`, `chat_id` | Explicit gateway key and positive private chat ID |

Without an explicit recipient, initiative goes to the configured owner's private
chat. Without an owner, initiative stays disabled with a warning until a recipient
is specified. For an explicit recipient, `interface` is the key from
`[gateway.<key>]`, such as `telegram`, and `chat_id` must be positive. Invalid
explicit recipients produce an error. The bot must be able to send to that chat.

Research also requires an active interest and at least ten minutes without
gateway activity. These are not configurable fields. See [Companion](companion.md)
for cancellation, budgets and daily-attempt semantics.

## Web research

Optional search and page reading are enabled together when `searxng_url` is set:

```toml
[research]
searxng_url = "https://search.example.com/"
obscura_path = "/usr/local/bin/obscura"
```

| Field | Default | Meaning |
| --- | --- | --- |
| `searxng_url` | None | SearXNG HTTP(S) base URL; absent disables both research tools |
| `obscura_path` | `"obscura"` | Local executable path or program name resolved from the service PATH |

Base URLs may include a reverse-proxy path prefix or private-network address.
Kodrik appends `/search` below that path. Credentials, query strings and fragments
are rejected; search requests do not follow redirects. No authentication headers
are configured by this integration. `obscura_path` must be nonempty and names one
executable, not a shell command with arguments. Unknown `[research]` fields are
rejected. Restart to apply configuration; see [Web research](web-research.md) for
SearXNG JSON setup, the Obscura requirement, limits and examples.

## Web pages

The server is disabled unless enabled explicitly:

```toml
[web]
enabled = true
listen = "0.0.0.0:8080"
base_url = "https://pages.example.com"
```

| Field | Default | Meaning |
| --- | --- | --- |
| `enabled` | `false` | Enable the page server in gateway mode and foreground publication tool |
| `listen` | `"0.0.0.0:8080"` | Internal HTTP socket address; numeric IP and port |
| `base_url` | None | Required when enabled; external HTTPS URL, optionally with a path prefix |
| `trusted_proxies` | `[]` | Exact proxy socket IPs permitted to supply a single client IP in `X-Forwarded-For`; never trust arbitrary clients |

`base_url` cannot contain credentials, query or fragment. Percent-encoded path
prefixes and double-slash paths are unsupported. HTTP is permitted only for localhost or loopback
development URLs. Proxy requests must preserve any configured path prefix.
Unknown `[web]` settings are rejected. Set the password using
`kodrik web set-password`; it is not stored in configuration. See
[Web pages](web-pages.md) for publication, access modes and proxy setup.

## Personality and system prompt

Edit `SOUL.md` in the current [space directory](storage.md#chat-spaces) to change personality, then start a fresh session.
The file is created once and preserved on later starts.

For an advanced base-prompt override, put `system_prompt` at the top level,
before any TOML table:

```toml
system_prompt = "Your custom base instructions."
```

This override applies only to CLI/TUI and the owner's private chat. Isolated chats use the default base prompt and their own personality and memory. The override still receives Kodrik's memory guidance and session context. Existing
sessions retain their saved snapshots. Restart and use `/new` to load changes.
