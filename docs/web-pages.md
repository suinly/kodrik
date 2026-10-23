# Web pages

Kodrik can create persistent native Nuxt UI pages for nutrition logs, reports, expenses
and other visualizations. Records are updated through conversation with the
agent. The browser displays the published data without calling the model.
The same server also hosts the [web chat](web-chat.md) at `/chat/`.

## Enable the server

Add a `[web]` section to `~/.kodrik/config.toml`. Settings and defaults are
listed in the [configuration reference](configuration.md#web-pages).

```toml
[web]
enabled = true
listen = "0.0.0.0:8080"
base_url = "https://pages.example.com"
```

Set the owner password interactively, then start the gateway:

```sh
kodrik web set-password
kodrik gateway
```

The password command does not require provider configuration. It reads the
password twice without echo; do not pass a password as a shell argument.
Changing it revokes all web sessions, including in a running server. Sessions
last seven days and survive restarts. Each identity keeps up to four browser
sessions; a new login replaces that identity's oldest session when necessary.
Telegram sessions share a pool of 64, with four additional slots reserved for
owner-password access. At capacity, an existing identity can replace
its own session; a new Telegram identity must wait for space. A login never
evicts another identity's session. Use the sign-out form at `base_url` to
revoke the current session. The configured owner can use the same Telegram login
for chat and private pages; password login grants the same owner access. Public
pages also work without login.

The server runs with the gateway. Terminal and `ask` modes can publish files
when web is configured, but do not start the HTTP listener. An empty gateway
table can be used for a web-only gateway; normal provider configuration is
still required by gateway startup.

## Reverse proxy

The built-in listener uses HTTP. Terminate HTTPS at your reverse proxy and
forward requests to Kodrik's port. Only the proxy should be able to reach
the internal port. Binding to `0.0.0.0` does not require exposing the port
directly to the internet.

For a dedicated domain, forward the whole domain. For a URL prefix, configure
`base_url = "https://pages.example.com/kodrik/"` and preserve that prefix:

```nginx
location /kodrik/ {
    proxy_pass http://127.0.0.1:8080;
    proxy_set_header Host $host;
    proxy_set_header X-Forwarded-For $remote_addr;
}
```

Configure the exact proxy socket addresses in Kodrik, for example:

```toml
[web]
trusted_proxies = ["127.0.0.1"]
```

Login attempts (password, code, and Telegram login start combined) are limited
to five per minute per client IP. By default, `trusted_proxies = []` and only
the actual socket peer is used; forwarded headers from other peers are ignored.
A trusted peer must send exactly one `X-Forwarded-For` value containing one IP
address. Missing, malformed, repeated headers and address chains are rejected.
The proxy must **overwrite** the header with the verified client address;
never use `$proxy_add_x_forwarded_for` or pass a client-supplied value through.
For multiple proxy hops, the final trusted proxy must resolve the client address
using its own explicitly configured trust chain before overwriting this header.
The limiter remembers at most 1024 active client IPs; additional clients receive
429 until a bucket expires (60 seconds). Existing buckets are never evicted.
Clients sharing a public IP also share its login budget.

The `proxy_pass` has no URI suffix, so `/kodrik/` is kept. Do not strip it.
Links, redirects, Origin checks and cookie attributes use the configured
`base_url`; forwarded headers do not override it. HTTPS is required in
`base_url`, except for HTTP on localhost or a loopback IP during development.
Disable proxy caching for this location; Kodrik also sends `Cache-Control:
no-store` on responses.

## Create and update a page

Ask Kodrik, for example: “Track my nutrition and create a private page with
the meal history and daily totals.” The agent uses the bundled `web-pages`
skill and calls `show_ui` with Markdown/MDC and named data sources. A nutrition page has a URL
such as `https://pages.example.com/pages/nutrition/`.

Source records live in [datasets](datasets.md), separately from presentation.
Adding a meal uses the dataset tool. Private pages read current values on reload;
public pages keep the published snapshot until the agent republishes them.
The same MDC document can appear directly in chat with frozen data and no page
URL. See [UI documents](ui-documents.md) for the component catalog, examples,
limits and interaction behavior. Durable record changes use web chat or declared
forms on private native pages; those forms save directly through the dataset
layer and refresh related views without an agent call.

### Legacy HTML pages

Existing custom HTML pages continue to use `publish_page` and drafts containing
`index.html`. They may include CSS, JavaScript, JSON,
images and nested resource folders. Asset URLs should be relative. Limits:
1000 files, 32 MiB total, 1000 directories and 32 levels of nesting. Symlinks
and special files are rejected. Failed publication leaves the previous
version available. Old releases are retained; there is no automatic cleanup.
See [Storage](storage.md) for locations and backups.

## Page appearance

Native documents inherit the app's standard Nuxt UI components and Catppuccin
Latte/Mocha theme with a Blue accent. Tables include local filtering/sorting;
private dataset-backed tables also provide bounded pagination. Graphs use a
small accessible SVG renderer with a readable data list. No CDN or page-specific
frontend build is needed; the renderer ships in the main web bundle.

Legacy HTML guidance defaults to calm minimalism: neutral light/dark surfaces,
an emerald accent, readable totals and restrained borders. Explicit user
design requests and existing approved designs take precedence. Pages use
version-pinned Tailwind Browser through jsDelivr; Chart.js is added through
the same CDN only when charts are useful. No frontend build is required.

Pages provide System, Light and Dark modes, with the preference shared across
pages in browser storage. Records remain in the published JSON files. Layouts
start at 320 px, with touch-friendly controls, responsive charts and tables
that scroll within their own region. Essential data remains readable if a
CDN library is unavailable. CDN scripts are trusted page code under the shared
origin model described below.

These are agent instructions for created or updated pages, not an automatic
restyling of existing publications. After deploying a rebuilt Kodrik with
the updated bundled skill, start a fresh conversation with `/new` and request
creation or a design update for the desired page.

## Private and public access

New pages are private by default. Private pages require owner login (password or
the configured owner's Telegram approval), shared with web chat, and all
associated data and resource files require the same session. Unauthorized
resource requests return 401 rather than the login form.

Ask explicitly to make a page public when it should open without a password.
Every file in that published page becomes public, including its JSON records.
Changing it back to private protects new requests, but cannot retract copies
someone already downloaded.

Legacy HTML pages share one origin, without iframe isolation. Treat their HTML and
JavaScript as trusted owner code. A public page opened by a signed-in owner
can access private pages using that browser session. HttpOnly prevents direct
cookie reading but does not prevent these requests. Do not publish untrusted
scripts or use this feature to host third-party code.

Native documents render a fixed component catalog after AST validation and do
not execute document JavaScript. Public native pages expose only the published
snapshot; live source reads and form writes are unavailable for public pages. This does
not change the shared-origin trust model of existing HTML pages.
