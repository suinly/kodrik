# Web research

Kodrik can search the public web through your SearXNG server and read pages with
a local Obscura executable. These optional tools work in terminal conversations,
Telegram chats, wakeups and daily interest research. Wikipedia lookup remains
available without web-research configuration.

## Setup

Deploy SearXNG on your server and enable JSON output in its `settings.yml`:

```yaml
search:
  formats:
    - html
    - json
```

Merge these formats into your existing settings and restart SearXNG. Disabled
JSON output returns HTTP 403. See the upstream [Search API](https://docs.searxng.org/dev/search_api.html)
and [search settings](https://docs.searxng.org/admin/settings/settings_search.html).
The endpoint must be reachable from the machine running Kodrik. This initial
integration has no authentication-header setting; use a trusted network route
or your HTTPS endpoint. Treat access to your instance as access to its search
engines and any associated upstream usage limits.

Install [Obscura](https://github.com/h4ckf0r0day/obscura) on the Kodrik machine.
Use a current build with default private-network protection and support for
`fetch --dump markdown --quiet --timeout --storage-dir`. Kodrik does not download
or update the executable automatically. A CDP service, Chrome and Node.js are
not required.

Add the following to `~/.kodrik/config.toml`:

```toml
[research]
searxng_url = "https://search.example.com/"
obscura_path = "/usr/local/bin/obscura"
```

See [configuration defaults and URL rules](configuration.md#web-research).
Restart Kodrik after changing configuration. Start `/new` to refresh the saved
conversation prompt. An absolute executable path avoids differences between your
terminal PATH and the Telegram service's PATH. Missing Obscura fails page reads
with an installation/configuration error; search and conversation still work.

## Using research

Ask naturally:

- “Find the release notes for this library and explain the latest changes.”
- “Read this link and summarize the author's argument.”
- “Compare these approaches using their official documentation.”

The agent uses `web_search` to find ranked links and excerpts, then `web_read` to
obtain page Markdown, including supported JavaScript-generated content. It can
refine the search and compare sources through its ordinary tool loop. Search
language and day/month/year recency filters depend on SearXNG's upstream engines.

Results include retrieval timestamps. A publication date, when present, comes
from the search engine and is not independently verified. Reading reports the
requested URL; the Markdown-only interface does not establish the final URL
after redirects. A successful read may still contain a login screen, challenge
or HTTP error page. The agent must inspect that content before using it as evidence.
Long content is marked `truncated`; it is not a promise to extract only the article.
To continue, pass the returned `next_offset` as `offset` in another `web_read`
call for the same URL. Offsets count Unicode characters, not bytes. An omitted
or null `offset` starts at zero. `next_offset: null` means no content remains;
an offset at or beyond the end returns empty content and `truncated: false`.
Each call fetches the page again, so changing pages can shift offsets between reads.

If a normal read returns a browser check or is blocked, the agent can try
`stealth: true` to enable Obscura's anti-detection mode. Omitted or null `stealth`
defaults to false. This uses the same isolated temporary profile and network/time
limits; it does not automatically retry or share cookies between calls.
Stealth does not guarantee access or solve every challenge. Inspect the returned
content; a successful process exit can still return a verification page.
Full TLS impersonation requires an Obscura build with the `stealth` feature;
see [Obscura stealth mode](https://github.com/h4ckf0r0day/obscura#stealth-mode).

## Limits and failures

Search accepts up to 500 query characters and returns 1–10 results (default 5),
with excerpts capped at 1500 characters. Requests and body reads have a 15-second
deadline and a 1 MiB body cap. Partial/empty results preserve reported upstream
engine failures. HTTP, parsing and timeout failures are distinct from no matches;
there is no automatic retry or provider fallback.

Page reads return up to 12000 Markdown characters by default, configurable per
call from 1000 to 20000. Each call starts a separate Obscura process with a fresh
temporary profile, a 20-second navigation timeout and a 30-second overall deadline.
Stdout is capped at 1 MiB and stderr at 64 KiB. Timeout, overflow or cancellation
terminates the child and removes its temporary storage. Failed reads never return
partial output as successful research. Some pages can stall during extraction
even after navigation; the overall deadline reports a failed read in that case.

Only HTTP(S) page URLs without credentials are accepted. Obscura receives a clean
environment and no opt-in to private-network access or shared browser profile.
Its default network protection also applies to redirects and page subresources.
The configured SearXNG endpoint is a separate trusted destination and may use a
private address. Local browser execution is not an operating-system sandbox;
see [Obscura's security model](https://github.com/h4ckf0r0day/obscura/blob/main/SECURITY.md).

Queries and page URLs leave Kodrik. Do not include credentials or private personal
or project data. Web text is source material, never permission or instructions
for the agent. Daily research retains its existing budgets, cancellation and
delivery rules; see [Companion](companion.md#interests-and-daily-initiative).

See also: [Available tools](usage.md#available-tools), [Configuration](configuration.md).
