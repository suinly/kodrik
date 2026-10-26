# Web chat

The agent discovers available Nuxt UI components through `ui_components` and
renders them using `show_ui` and MDC. These components inherit the chat theme.
Inputs, tabs and overlays keep local state; charts use Nuxt UI's UChart.
Private published forms can insert/edit dataset records without invoking the
agent; form previews in chat cannot save. Prompt buttons prepare a
follow-up in the composer for review; they do not submit automatically. Chat
history preserves the shown data snapshot. The same document can be published
as a persistent page; see [UI documents](ui-documents.md).

Personal conversations belong to a user and are shared between web chat and
private Telegram chats. CLI/TUI always uses the configured owner, whose web agent
also shares terminal tools, memory and skills. Access is by invitation; groups are disabled.
Guest mode in private chats gives each pair shared context and sandboxed tools,
separate from their personal spaces, for the owner and invited users.
The Nuxt 4 SPA uses standard Nuxt UI components and renders canonical snapshots; Axum
handles authentication, persistence, agent execution and SSE subscriptions. No Node.js server is needed in production.

## Build and start

Install Node.js 22.22.3+ (22.x), 24.15+ (24.x), or 26+, and a current stable Rust toolchain, then build from the
repository root in this order:

```sh
npm ci --prefix web
npm run build --prefix web
cargo build --release --locked
```

Frontend assets are embedded in the executable. Rebuild both frontend and Rust
after frontend changes. A build without frontend assets fails immediately with
instructions to build them, before an updater can replace a working executable.
The macOS installer performs both builds.

For a local server, add this to `~/.kodrik/config.toml` alongside your provider
configuration and `[gateway]` table:

```toml
[web]
enabled = true
listen = "127.0.0.1:8080"
base_url = "http://127.0.0.1:8080/"
```

```sh
./target/release/kodrik web set-password
./target/release/kodrik gateway
```

Open `http://127.0.0.1:8080/app/chat`. For remote access use HTTPS through a reverse
proxy, following [Web pages](web-pages.md#reverse-proxy). With a configured
prefix such as `/kodrik/`, the chat is at `/kodrik/app/chat`.

Dashboard sections use `/app/chat`, `/app/pages`, `/app/webhooks` and `/app/profile`. A selected
object appends its session ID, page slug or webhook ID to the section path.
Old `/chat/` links with selection query parameters redirect to these paths.

The sign-in page uses the same Nuxt UI components and Catppuccin theme as the
chat. Choose **Войти через Telegram**, then open the bot with the button or scan
the QR code on another device. Compare the six-character code and site shown in
the bot with the original tab, then press **Войти** or **Отказать**. Approval
signs in the original browser automatically; the request expires in five minutes.
The gateway discovers its configured bot's username at startup, without extra
BotFather settings. The QR code is generated locally.

The owner can also use the configured password. Telegram approval authenticates
the approving user and never grants access to another user’s history.
If no bot is available, use the password form. Set an owner password with
`kodrik web set-password` if one has not been configured.

Each identity keeps up to four browser sessions; signing in again can replace
its oldest session. Telegram identities share 64 slots, with four additional
slots reserved for owner-password access. Existing identities can
replace their own sessions when the pool is full; other identities' sessions
are preserved. Login attempts share a five-per-minute budget per client IP
across password login, Telegram login start and profile password changes. Configure trusted proxy addresses
and header overwriting as described in [Web pages](web-pages.md#reverse-proxy).

## Profile and password

Open **Profile** in the section navigation (`/app/profile`). Each admitted user
can save their own display name, containing 1–100 characters. Role and linked
Telegram ID are read-only. The owner shares one profile between password and
owner-Telegram sessions; invited users have separate profiles.

Only the owner can set or change the owner password. Enter the current password
when one exists, then enter the new password twice. An owner signed in through
Telegram can set the first password without using the CLI. Invited users keep
Telegram sign-in and cannot manage the owner password.

New passwords require at least 15 characters.
Whitespace-only values, repetitions of 1–4 characters and a small local
list of common passwords are rejected. Use a unique, randomly generated password
or a long passphrase; this check is not a complete leaked-password database.
Uppercase letters, digits and symbols are allowed but are not mandatory.
Existing passwords continue to work until changed. The same requirements apply
to `kodrik web set-password`.

Changing the password revokes **all** browser sessions, including invited users,
and pending Telegram sign-in approvals. The current browser returns to sign-in
with a confirmation message. Already admitted agent tasks are not cancelled.
Rejected changes preserve the password and existing sessions.

Profile API: `GET /api/profile` reads the current profile, `PATCH /api/profile`
updates `{ "name": "Alice" }`, and owner-only `POST /api/profile/password` accepts
`current_password`, `new_password` and `confirm_password` strings. All endpoints
require a valid session, and writes require the configured Origin. Password
changes share the five-attempts-per-minute client-IP budget with login; excess
attempts return 429 with `Retry-After: 60`.

## Owner-managed invitations

Access is closed by default. The local service owner creates a separate single-use
link for each person; users cannot issue invitations or register themselves:

```sh
kodrik invites create --label "Alice" --expires-in-hours 168
kodrik invites list
kodrik invites revoke INVITE_ID
kodrik invites revoke-user TELEGRAM_USER_ID
```

Run these commands on the server as the account owning `~/.kodrik/`. Creating a
link requires enabled `[web]`, a valid `web.base_url`, and a configured Telegram
bot. Start the gateway before the recipient opens the link. The default lifetime
is seven days; the allowed range is 1–720 hours. Expiry limits redemption, not
access already granted. The console prints the person’s label, the URL on its own line, a local expiry
date, and a revoke command. The URL is printed once; the store retains only a hash.
New links use `/i#<code>` with a 22-character random code (128 bits); existing
long links remain valid.
There is a maximum of 1024 retained invitations and 1024 admitted users.

Send that person's URL privately. It opens a Telegram-only invitation screen.
The recipient opens the bot, verifies the site and confirmation code, approves,
and returns to the original browser. Redemption binds access to the approving
Telegram ID and consumes the link atomically. The label is for the owner's records;
it does not verify who received the link. Whoever redeems it first gets access.
A forwarded or leaked link should be revoked and replaced.

The secret lives in the URL fragment, which is not sent in HTTP requests; the
page removes it from browser history and keeps it only in memory for login.
After reloading before redemption, reopen the original invitation link.
Once admitted, the person can sign in normally through Telegram and use the bot
in private messages. They receive isolated user tools, never owner privileges.
The configured `[owner]` Telegram ID and owner-password login need no invitation.
Existing Telegram users and sessions also require admission after this upgrade.
Groups are disabled. The owner and invited users can invoke guest mode in private
chats with other people. Guest access is checked against the current sender. Both
participants use the same guest history, memory, datasets and virtual filesystem;
other pairs have separate spaces. The agent sees each sender's Telegram user ID,
name and username, plus supplied reply context. Guest
runs never use personal history, memory, skills or native shell access, including
when invoked by the owner. Revoking access also blocks guest requests and replies.

Revoking the invitation also revokes the access it granted. Revoking a user
invalidates web sessions and pending approvals. New bot messages,
web requests and model attempts (including background work and retries) are denied.
An already sent provider request or an already executing tool cannot be undone.
Stored user history is retained. Re-admission requires a newly generated invitation.
Published public pages remain public; private owner pages remain owner-only.
Billing, credit balances, tariffs and payment integration are not used.

## Conversations

On mobile, open the sidebar with the menu button or swipe right from the left
edge. It overlays the current screen, leaving a dimmed strip visible; tap that
strip or press Escape to close it. Vertical scrolling and conversation drafts
are preserved.

The sidebar has **Conversations** and **Pages** tabs. Pages lists the owner's
existing publications with title/address search and public/private labels.
Selecting a page opens it in the main chat panel, keeping the current conversation
and draft available. Use Back to conversation to return, or Open in a new tab
to view the standalone page. Native pages use the existing live document renderer;
legacy HTML pages open in an iframe under the existing trusted-owner-code model.
The catalog reloads when opened; use Refresh to pick up changes while viewing it.
Native pages use their published titles; legacy HTML pages use their URL slugs.
The catalog requires login and is empty for invited users, who cannot publish
pages. `GET /api/pages` returns this authenticated catalog as an array of
`{slug, title, url, visibility, document}`; signed-out requests receive 401.
The owner's password login and configured-owner Telegram login both grant access
to the catalog, private pages, live data and forms using the same chat session.
Invited users cannot access the owner's private pages.

The chat start screen shows the regular composer centered in the main panel,
including file/photo attachments. Sending the first message creates a conversation
and moves the same input to the bottom, keeping keyboard focus. Text survives a
reload; a failed creation keeps the draft available for retry. The movement
respects reduced-motion preferences.

Create an empty conversation or select an existing one from the sidebar. A short title
is generated by the configured LLM in the background after the first answer,
in the conversation's language, and saved separately from its history. Sidebar
and navbar update automatically. Until then, or if naming fails, the title is
the first user message (or “Новый диалог” for an empty history). Existing
conversations get a generated title after their next web or private Telegram answer.
Telegram uses the same saved titles, including manual names and branch titles.
Generated titles stay stable across messages and server restarts. Naming uses
bounded text excerpts and attachment names/counts without tools or attachment
contents; it does not delay replies or occupy an executing conversation slot.
While naming is in progress, `UChatShimmer` animates the title in the sidebar and
navbar independently of the agent's running indicator; it stops on success or failure.
Enter sends;
Shift+Enter inserts a newline. The sidebar shows short titles, last-message
date/time and search across titles and user/assistant message text, including
preserved versions. The sidebar loads 30 conversations initially and fetches the
next page near the bottom while scrolling. Search results use the same paging;
search still checks the complete saved history. “Показать ещё” provides a keyboard
alternative, and failed loads can be retried. Results show a matching excerpt and open the matching branch.
The search button in the header opens a field above the transcript, which moves between matching messages with Enter
or the arrow buttons and highlights the current message. The full title remains
available on hover.

The sidebar's conversation menu appears on hover or keyboard focus, stays visible
while open, and remains available on touch screens. It can pin/unpin or rename a
conversation, or delete it after confirmation. Pins are saved on the server for
all versions of a conversation and survive reloads and restarts. Pinned
conversations appear first, retaining the existing activity order within each group,
including search results. A manual title applies to all versions and is never overwritten
by background naming. Deletion removes all branches, saved runs, queue records
and private uploads for that conversation. Finish or stop running work and drain
any paused queue before deletion. Other open tabs detect the deleted conversation.
The pencil beside the header title opens the same rename form. It appears on
title hover or keyboard focus and stays visible on touch screens.

Message controls sit below the bubble without a background. They appear on hover
or keyboard focus and stay visible on touch screens.
Use the pencil below a user message to edit its text, then “Сохранить и
отправить” to generate a new answer. The history before that message and its
attachments are retained. The original message and all subsequent conversation
remain in the old branch; arrow buttons below the message switch versions.
You can edit earlier messages and edit an already edited version. Each branch
accepts follow-ups, and the sidebar remembers the last selected branch. Editing
is available when the conversation's branches are idle and their queues empty.
Editing or deleting history does not undo tool actions or remove published pages.
Responses stream as they arrive. Chat messages have no avatars. Consecutive tool
steps share a collapsed `UChatTool` activity group; expand it and a native
`UChatTool` step to inspect arguments, output or errors.
Messages longer than 1,000 characters or 12 lines start with a short preview.
Expand a message to read its full Markdown or webhook payload, then collapse it
again for a compact transcript. Search reveals the selected matching message;
live replies remain expanded while streaming. Editing uses the full original text.
Chat errors use compact red text with recovery actions beside them.
Earlier document snapshots with the same published URL are collapsed as previous
versions. Each preview identifies its data as a creation-time snapshot, with the
live-page link above it. Full-screen document viewing preserves local controls.

Each conversation keeps its own unsent text and selected attachments when
switching within the app. Text survives reloads in this tab's session storage;
after reload, file names are shown as a reminder to reselect their bytes.
While the agent works, the composer accepts a draft and attachments for the
next message. Send admits text, files and photos immediately. Like personal
Telegram messages, they join the active run after its current tool step finishes,
so the agent can use them in the current answer. A running model request also
finishes before steering is consumed.
Pending messages appear as user bubbles in the transcript immediately after
admission, with attachment names and a waiting status. They survive reloads and
become canonical history as the agent consumes them. File downloads and photo
previews become available after consumption. While busy, the composer shows Stop
when empty, or Send when it contains text or attachments. A native `UChatShimmer`
shows “Кодрик работает…” in the transcript while busy. Keyboard hints and other
run statuses sit below the composer. A borderless `UAccordion` plan sits above
the prompt and shows the current step or completed-step count.
The input stays one row high, with upload on the left and an icon-only Send/Stop
button on the right. Long text scrolls within the field; Shift+Enter and pasted
newlines are preserved. The controls retain accessible names.
Scrolling up pauses automatic following; “К последнему сообщению” returns to the
newest content.
Its transparent overlay floats over the transcript without reserving a footer row.

The composer receives focus once after opening a conversation, including creating,
switching and reloading, as soon as its history is loaded and input is available.
Subsequent updates do not steal focus. Typing a printable character outside an
editable field returns focus to the visible, unlocked composer and inserts that
first character at its saved selection. Shortcuts, composition, open dialogs and
menus, and Space on a button or link keep their normal behavior. Mobile Safari
may require a tap before showing the software keyboard.
On narrow screens the composer stays compact, its touch targets remain large, and
keyboard hints are hidden. The chat and mobile sidebar respect display safe areas;
the visual viewport keeps the composer above the software keyboard.

## iOS Home Screen

In Safari, open the chat, choose Share → Add to Home Screen, and leave Open as Web
App enabled when offered. Kodrik provides a manifest with `display: standalone`,
a stable chat start URL and PNG icons (including an Apple touch icon). All asset
and manifest URLs work under the configured base prefix; login and private pages
remain inside the app's scope. Manifest and icons are public, conversation APIs
and attachments still require login. This is an online app; no service worker or
offline conversation cache is installed.
Following also tracks late image/tool layout changes and viewport resizing.

Use the paperclip to select files, drop them onto the composer, or paste a photo
from the clipboard. The upload list shows previews and lets you remove individual
files before sending. Attachments can be sent with or without text. Photos appear
in history; file links download the original bytes after authentication.

The interface follows the system color scheme, with a switch in the sidebar:
Catppuccin Latte in light mode, Mocha in dark mode, and Blue as the accent.
Component spacing, buttons, panels and controls use Nuxt UI defaults.

Stop persists cancellation and waits for a snapshot confirming cancellation or a
terminal result. Closing a tab, changing conversations, logging out, or losing
the network closes only the observer: admitted work continues on the server.
The selected conversation is saved in the URL. Reloading reads its current
snapshot and subscribes again, restoring history, tool cards, plan, queued
inputs and partial text. Unconfirmed text from an interrupted attempt is marked
separately; it is never appended to a replacement attempt or a confirmed answer.
Network loss shows “Соединение потеряно, работа продолжается”. Reconnect uses
backoff from 500 ms to 8 seconds. A 401 stops reconnect and offers login.

Each send has a UUID saved in this tab's session storage before its first HTTP
request. After an uncertain response, another Send checks the request's saved
status first. Already accepted work is not submitted with a fresh UUID. A
confirmed missing request can be retried using the same UUID; a failed status
lookup keeps the original identity for the next check. After loading a terminal
result, a subsequent explicit Send is a new request.

For attachments the pending identity also includes content fingerprints. After
a reload, the browser checks the saved request before resubmitting. If the server
never accepted it, the user must reselect the files; file bytes are not stored in
session storage. A failed send keeps the selected files while the tab stays open.

All API paths below are relative to the configured base prefix:

- `GET /api/chat/sessions` lists the authenticated user’s personal conversations with `id`, generated
  or manual `title`, `root_id`, `title_pending`, `busy` and nullable ISO
  `updated_at` (last user/assistant message time across versions). One row is
  returned per conversation; `id` identifies its selected branch. Optional `q`
  searches case-insensitive literal text in titles and user/assistant messages,
  returning `snippet` and `match_message` for content hits. Tool payloads and
  attachment bytes are excluded. `limit` defaults to 30 and accepts 1–100;
  `offset` is a nonnegative row offset (default 0), applied after pin ordering
  and branch grouping. The response remains an array; a short page is the end.
  Normal browsing decodes histories only for that page's conversation groups.
- `PATCH /api/chat/sessions/{id}` accepts `{title}` (1–200 characters after
  trimming, no control characters) and renames all branches of the conversation.
- `DELETE /api/chat/sessions/{id}` deletes the conversation and all its branches
  and private uploads, returning 204; running work or pending queue items return 409.
- `POST /api/chat/sessions/{id}/select` remembers the selected branch, returning 204.
- `POST /api/chat/sessions/{id}/pin` accepts `{pinned: boolean}` and pins or unpins
  the entire conversation, returning 204. Session list entries include `pinned`.
- `POST /api/chat/sessions/{id}/messages/{message}/edit` accepts
  `{request_id, text}` and returns `{id}` for a new branch. `message` is the raw
  history index from a user message ID (`session:index`). A canonical UUID
  `request_id` is also the new branch ID. Repeating the same edit is idempotent;
  changed input with the same ID returns 409. Branch history and admission commit
  atomically. Attachments stay attached; the text keeps the 32 KiB limit.
- `POST /api/chat/send` accepts `request_id`, `id` and `text`, and returns JSON
  `{request_id, submission_id, run_id, status}` after durable admission. `run_id`
  is null until the first checkpoint binds an idle request; steering during a
  live run is bound at admission. Repeating the same input
  returns its saved identity/status; changed content under the same ID returns 409.
  It also accepts multipart form data with `id`, `request_id`, `text`, and repeated
  `files` fields. JSON text clients continue to work.
- `GET /api/chat/sessions/{id}/attachments/{message}/{kind}/{index}` downloads a
  saved user attachment (`kind` is `files` or `images`). The server resolves it
  from session history; host filesystem paths are never accepted or exposed.
- `GET /api/chat/sessions/{id}/requests/{request_id}` returns the saved status,
  run ID and queue-blocking reason.
- `GET /api/chat/sessions/{id}/run` returns one canonical conversation snapshot,
  including revision, latest run (or null), messages, busy/cancellation state,
  plan, live partial, queued inputs and delivery status. `title` contains the
  generated name, first user message or “Новый диалог”; `titles_revision` signals sidebar title
  changes across conversations, including generation start/end. `title_pending`
  distinguishes title generation from the main agent's `busy` state.
  User messages with edited versions also expose ordered branch IDs in `versions`
  and the zero-based selected `version` index.
- `GET /api/chat/sessions/{id}/events` is a read-only SSE subscription. Each data
  frame replaces the snapshot; it does not append text. Reconnect starts with
  the current snapshot. Persistent revisions suppress stale/duplicate frames.
- `POST /api/chat/sessions/{id}/stop` persists a control command and returns 202
  after applying it. Other observers see the same cancellation state.
- `POST /api/chat/sessions/{id}/resume` clears a stopped queue's block and starts
  processing its existing messages without admitting another item. It returns
  202; disconnecting the observer does not abandon the transition. It uses the
  same authentication, Origin check and conversation capacity limit as Send.

All endpoints require login; mutations also require the configured Origin.
The observer rechecks authentication while connected. Older clients that omit
`request_id` get a new server ID and cannot safely deduplicate retries. The old
AI SDK chunk stream on POST send has been replaced with JSON admission.

Failed or stopped work leaves independent queued follow-ups paused. The history
response exposes `blocked_reason`; “Продолжить очередь” resumes existing items,
including a full queue. New input also resumes them in FIFO order when there is
room for that input. Stop cancels the current run, including pending steering
bound to it. Independent follow-ups saved by older clients remain paused in the
queue; retries preserve each saved submission's original admission mode.

SSE subscriptions have a separate pool of 64 streams and a limit of four per
user across all browser tokens and conversations. Owner-password and owner-Telegram
access share the same stream limit. Excess streams receive 429 per user or 503
when the stream pool is full; disconnecting releases capacity. Streams do not
occupy the separate pool of 64 ordinary HTTP responses, so login and normal API
requests remain available when SSE capacity is exhausted.

The server allows four executing web conversations per user, one run per session.
Concurrent work in another interface returns 409. Each interface retains its
own active-session selection, and replies go only to the interface that accepted
the request; a web prompt does not send an answer to Telegram. In Telegram,
`/sessions [page]` lists personal dialogs and `/session <id>` selects one with
a short confirmation, without replaying the transcript. A paused queue must be
resumed through its original interface before another interface can add work.
An already active conversation accepts up to 100 queued messages even when all
four worker slots are occupied. New conversation execution and inbox overflow
return 429; a duplicate accepted request remains idempotent at these limits.
Recovered work reserves capacity before HTTP admission; excess recovery jobs
wait for a slot. Provider retry waits occupy their conversation's slot.
Messages are limited to 32 KiB of UTF-8 text, 10 attachments and 50 MB total upload.
Multipart sends have a 300-second request timeout; other requests retain the
10-second timeout. Once admission starts, disconnecting does not cancel it.
Documents are limited to 20 MB each, photos to 10 MiB. Supported files use the
agent's existing allowlist: PDF, documents, presentations, spreadsheets, text and
code. Photos support PNG, JPEG and WebP and are decoded for validation. Voice
and replacing existing attachments during an edit are not supported. Web sessions are selected explicitly; automatic
topic switching does not apply.

History lives in the authenticated user's `~/.kodrik/users/<telegram-user-id>/sessions/`
store, shared with private Telegram chats. The configured owner also shares CLI/TUI
history. Without a configured Telegram owner, local owner history remains in
`~/.kodrik/sessions/`. Existing web history and its metadata migrate once; old
archives and absolute attachment paths remain intact. Back up the whole data root.

## Access and proxy streaming

Password login grants owner-level agent access, including native shell and file
tools. Telegram approvals bind chat access to that Telegram user;
other users retain isolated tools and cannot access owner pages or datasets.
API mutations require the configured Origin. Approval requests also require an
independent HttpOnly, SameSite=Strict cookie in the initiating browser; the public
bot link alone cannot sign in. Cookies use the HTTPS secure flag when configured.
Changing the password revokes sessions and pending approvals and closes existing
observers; it does not undo or cancel an admitted run.

Published pages and chat share an origin. As with private Pages, published HTML
and JavaScript must be trusted owner code: scripts on those pages can also call
the chat APIs using a signed-in owner's cookie. Do not host untrusted scripts
on this origin. Third-party publication would require separate-origin isolation.

For nginx, preserve the configured prefix and disable response buffering:

```nginx
location /kodrik/ {
    proxy_pass http://127.0.0.1:8080;
    proxy_set_header Host $host;
    proxy_buffering off;
    proxy_read_timeout 300s;
}
```

The stream also sets `X-Accel-Buffering: no`, sends keepalive events and uses
`Cache-Control: no-store`. Markdown is sanitized; raw HTML and MDC component
syntax are disabled. The HTML response grants a fresh nonce to Nuxt bootstrap
scripts. Nuxt UI requires inline style attributes; inline scripts without the
nonce remain blocked. Configure any reverse proxy upload limit to allow 51 MB
requests (for example, nginx `client_max_body_size 51m;`).

If a known accepted request is queued behind a stopped/failed run, editing the
composer and sending new text resumes FIFO processing under a new request ID.
Retrying the unchanged pending text only checks its status. Earlier follow-up
answers remain in history and the observer follows the current run as the queue
drains. Stop cancels whichever run is active during this drain and fences the
gap before a newly admitted request's first checkpoint.

## After a server crash

Reloading reconnects to the durable conversation; it does not submit the prompt
again. Saved partial text from the interrupted attempt is separate from the new
attempt. An accepted request keeps its original UUID even when the admission
HTTP response was lost. Parked runs need operator investigation and explicit retry
or cancellation in the `web` store. A disconnected browser is not a Stop request.
See [crash evidence](durable-recovery.md) and the
[operator recovery procedure](operations.md#recovering-after-a-crash).
