# Remote TUI and temporary laptop access

Remote TUI connects to the owner's running gateway. The agent, model credentials,
memory, conversations and background tasks stay on the server. The laptop runs
the terminal client and a temporary local executor, not a second agent. There is
no history synchronization or automatic project upload.

This mode is owner-only and currently Unix-only. Invited users, guest spaces and
groups do not receive the laptop environment. The same owner conversation can
be opened in Web Chat or the owner's personal Telegram chat. Selecting another
conversation in one interface does not move the environment to it.

## Configure and pair

On the server, configure the model and enabled web server as usual and keep
`kodrik gateway` running. Owner sign-in is needed for pairing; see
[Configuration](configuration.md) and [Web chat](web-chat.md).

On the laptop, create a separate `~/.kodrik/remote.toml`:

```toml
server = "https://kodrik.example.com/"
label = "My laptop"
credential_file = "remote/device.json"
allow_shell = false
skill_roots = []
optional_mcp = []
```

There is no `[provider]` or `[gateway]` in this file. Pairing and remote TUI do
not load the standalone `config.toml` or require a local LLM key. Unknown remote
fields are rejected. Values are literal: neither `${VARIABLE}` nor `~` is
expanded inside paths. Relative credential and skill-root paths resolve from
the remote configuration's directory, not the project directory.

| Setting | Default / meaning |
| --- | --- |
| `server` | Required HTTP(S) base URL, with a trailing `/` |
| `label` | `"Laptop"`; display label, not an identity or permission |
| `credential_file` | `"remote/device.json"`, relative to the config file |
| `allow_shell` | `false`; enable local shell explicitly |
| `skill_roots` | `[]`; directories containing selected `<name>/SKILL.md` folders |
| `mcp` | Empty; selected laptop-side Streamable HTTP MCP servers |
| `optional_mcp` | `[]`; names from `mcp` whose connection failure may be omitted with a warning |

HTTPS is required except for loopback HTTP, such as `http://127.0.0.1:8080/`.
Do not put credentials, a query or fragment in `server`. A reverse-proxy prefix
is supported, for example `https://kodrik.example.com/kodrik/`; use the final
base URL and proxy its API, SSE and WebSocket routes without redirects.

Protect the configuration, especially if adding MCP headers:

```sh
install -d -m 700 ~/.kodrik
chmod 600 ~/.kodrik/remote.toml
kodrik connect
# Or choose a different client configuration:
kodrik connect --remote-config /absolute/path/to/remote.toml
```

The terminal prints the server profile URL (`/app/profile` beneath the base URL),
a request UUID and an eight-character comparison code. Sign in there as the
owner, open **Device connections**, enter the UUID and code from this terminal,
compare them, check the comparison confirmation and approve. Approve only a
request you initiated. Pairing expires after five minutes; the one-use token is
saved to `credential_file` with mode `0600` in a private directory. Device
credentials expire after 30 days and are not owner passwords or model keys.

Use **Revoke access** for that device in the same profile to revoke its active
connections. Closing the profile page does not revoke anything. There is no
device-revoke CLI command. Pairing never overwrites an existing credential:
after revoking an old or expired device, explicitly remove its local credential
before pairing again. If token delivery was interrupted, inspect/revoke the
possibly registered device before starting a new request; an uncertain token
response is not automatically retried.

## Launch, select and attach

Start in the laptop project directory you intend to share:

```sh
cd /path/to/project
kodrik --remote
# Or:
kodrik --remote --remote-config /absolute/path/to/remote.toml
```

An interactive terminal is required. The initial server conversation is the
first in the returned session list, or a new conversation if none exists.
The launch project attaches automatically. For a new or changed environment
grant, the TUI first shows the server, project and capabilities with an
**Allow / Reject** keyboard picker, like a command approval. Use arrows and
Enter; Esc rejects. Rejection keeps server chat available without local access.
An unchanged saved grant needs no further confirmation.

| Remote TUI input | Action |
| --- | --- |
| `/sessions` | Select a saved server conversation; load 30 at a time without blocking keyboard input |
| `/new` | Create and select a server conversation |
| `/attach` | Reconnect the launch project; show the grant picker if approval is needed |
| `/detach` | Revoke this environment binding and clean up local work; keep server chat available |
| `/retry` | Check an unconfirmed message/edit using its original request ID; not a tool retry |
| `/older` | Load another server history page; PageUp also requests older history |
| `/audit`, `/audit trusted`, `/audit review`, `/audit ask`, `/audit status` | Inspect/change the current server session's command audit mode |
| `/approvals`, `/approve <id>`, `/deny <id>` | Refresh pending approvals or decide one for this session |
| Esc while working, with no menu/editor open | Stop the current server request |

The session picker opens immediately with a loading indicator. Near the end,
it loads the next page automatically. Esc closes it even during a request;
on a list error, existing rows remain available and `r` retries that page.
Use matching client and gateway versions: older gateways do not support this
paginated session-list API.

In slash-command suggestions, Enter runs the highlighted command directly;
Tab only completes its name. `/language` and `/audit` open keyboard choices.
Bare `/approve` and `/deny` open the full pending-call review, without deciding
anything until a separate explicit choice.

The terminal reviews one pending approval at a time and preserves its complete
arguments. `/approve` and the trust shortcut require that approval to be
displayed first. If an approval exceeds the 512 KiB terminal review envelope,
the TUI keeps the busy state and approval ID but disables Allow: review the full
call in Web Chat or reject it with `/deny <id>`. Live output and operation
evidence have separate projection limits and cannot remove Stop or approval
controls; full durable results remain in server history.

The project root is the canonical launch directory. File read, patch and image
tools are included; shell is included only with `allow_shell = true`. A new
manifest grant discloses exact tools, selected MCP, skills and project
instructions inside the full-screen UI, with PageUp/PageDown for long details.
The saved `.grant` fingerprint beside the credential avoids repeating the
picker for an unchanged configuration/manifest. Shell remains opt-in.

Selecting a conversation, creating one, editing a message into a new branch or
switching message versions **detaches the old binding before switching**, then
automatically attaches the selected conversation without another approval for
an unchanged grant. The previous conversation loses local access. At most one
environment can be attached to a session, and one connection serves one session.

The remote footer shows the audit mode, model, project basename and connection
state, without the session UUID, device label or full project path. `connected`
means both server chat and this TUI's local binding are connected; `detached`
means server chat is available without a local binding; `disconnected` means
server observation or a previously attached local binding has lost connection.
Use `/attach` to recover a failed local binding. On narrow terminals connection
state takes priority over model/project details. Standalone TUI has no remote
connection label.

While attached, new explicit owner messages in that same server conversation
can use the environment from TUI, Web Chat or personal Telegram, subject to the
same permissions and command audit. Tool/approval details identify the device,
project and connection generation. An old approval does not authorize a new
generation. After detaching or closing TUI, the owner can continue the
conversation in web/Telegram, **without access to the laptop**. Server tasks
remain on the server; local operations never silently fall back to server shell.

`kodrik` without `--remote`, `kodrik --tui-slot N`, `kodrik ask "…"` and
`kodrik gateway` retain their standalone behavior. `--remote` cannot be combined
with those subcommands or `--tui-slot`; there is no remote `ask` mode. Remote TUI
supports the commands above and local interface-language commands, not every
standalone slash command.

## Local skills, MCP and data disclosure

For example, extend `remote.toml` with selected providers:

```toml
allow_shell = true
skill_roots = ["laptop-skills"]
optional_mcp = ["local_docs"]

[mcp.local_docs]
type = "http"
url = "http://127.0.0.1:9000/mcp"

[mcp.local_docs.headers]
Authorization = "Bearer REPLACE_WITH_LOCAL_TOKEN"
```

Replace the corresponding top-level settings rather than duplicating them, and
keep them before the MCP tables. The example skill directory is relative to
`remote.toml` and must exist; put skills in `laptop-skills/<name>/SKILL.md`.
Omit `optional_mcp` for a required provider: failure then prevents attachment
rather than silently dropping it. Reopen the remote TUI after configuration
changes, then explicitly attach again.

Local skills and the launch project's `AGENTS.md` form a temporary live context
layer, not a server installation or a permanent replacement of server skills.
Local skill names may overlap server names; select by origin and full path.
The catalog contains metadata and hashes; skill text/resources are read on
demand on the laptop. Changed skill/project instruction contents invalidate the
catalog and require explicit detach/reattach. Old text can remain in history or
compaction, but it does not restore active instructions or permissions.

MCP connections and their transport headers stay on the laptop. The server gets
selected schemas/results, not an imported secret-bearing MCP configuration.
Server and laptop MCP use the shared `mcp_search` with distinct tool origins.
Only Streamable HTTP MCP is supported: **no stdio MCP** or local MCP process
launcher is provided by remote mode.

**Attaching discloses local data.** Device/project paths, tool/skill metadata,
project instructions, tool arguments, requested file/image contents, skill text
and command/MCP outputs can reach the server and its configured LLM provider and
remain in conversation/checkpoint history. This is not bulk project mirroring,
but output containing secrets is not automatically redacted. Review what you
share before granting access.

File/patch/image operations check allowed project/skill roots and reject path
escapes. These checks do **not** sandbox shell or MCP: shell runs with the local
user's privileges and inherited environment; MCP acts with its service/token
permissions. Either can affect files, networks and services outside the selected
project. Choosing a working directory is not an OS security boundary.

## Disconnect, sleep and uncertain outcomes

The environment uses heartbeats every **5 seconds** and a **15-second lease**.
Disconnect, lease expiry or detected suspend/resume stops admission and initiates
cleanup. Normal exit, SIGTERM and explicit detach do the same. A child supervisor
watches the TUI lifetime pipe, including EOF after SIGKILL; it is not a persistent
daemon and accepts no new work after the TUI disappears.

Cleanup is **best effort**: owned shell process groups receive SIGTERM, then
SIGKILL after a two-second grace period. This covers ordinary children, not
arbitrary descendants escaping with `setsid`/double-fork. Network partitions,
OS suspension and scheduling can delay detection; after wake, a shell may run
before cleanup gets CPU time. There is no freeze-before-resume guarantee.
Cancelling an HTTP MCP call cannot prove that its external service stopped.
Already-written files, commits, installations or external actions are **not
rolled back**. Killing a process is not proof that it caused no effects.

Chat observation reconnects and refreshes server history automatically, but the
local environment does not: `/attach` and `/attach yes` establish a **fresh
generation**. Neither reconnect nor reattach automatically resumes paused local
work. A dependent run enters `EnvironmentPaused`; saved results remain available
and a new explicit message can continue the discussion or request new work.

Server intents and laptop receipts distinguish known results from `unknown`
outcomes. Old receipts may reconcile evidence after reattach; they do not renew
old access or replay commands. Remote tool effects are not automatically replayed
even if a result was lost. Check the external system before requesting a new
effect. Keep the laptop receipt journal if reconciling an incident.

If sending/editing a message failed ambiguously, do not paste it again as a new
request. `/retry` first asks about the original submission ID and submits that
same input only if the server has no record. This is message deduplication,
not permission to re-execute a tool or a claim of exactly-once external effects.

## Attachments, migration and backups

Remote TUI currently has **no attachment-upload UI**. It renders saved file
references as “available in server history”, not as a local download/preview.
Open Web Chat to view/download a saved server attachment. A laptop pathname is
not a server URL and a disconnected file cannot be reopened from that path.

A requested UTF-8 `read_file` result (up to 1 MiB) may be saved separately when
the tool is called with `save_attachment = true`. Only a completed result with
that explicit flag creates a server `FileAttachment` (empty text creates no
downloadable file); ordinary reads do not automatically create downloadable
files. Their text can still remain in tool
history and be sent to the model. Server bytes, size and hash back the saved
reference. Unsaved local-file rows identify that the file remains on the device
when disconnected. Existing images retain their `Image.data_url` in session
JSON (up to 10 MiB per image); this does not add a remote TUI uploader.

The current session SQLite schema is **v13**. Opening an older supported schema
upgrades transactionally: v12 adds remote bindings/operations; v13 adds saved
remote-tool attachment references. Existing legacy JSON import remains a
one-time transaction and preserves original JSON bytes; imported JSON is not
subsequently authoritative. Remote mode does not copy an existing laptop's
standalone history or memory to the server. Pair/select server history rather
than treating the laptop's state as a synchronization source.

The existing owner-space migration carries remote bindings/operations from v12
and attachment metadata from v13, preserving immutable references. Referenced
source attachment files are retained, as with earlier user files; this is not
an automatic file relocation or garbage-collection pass.

Back up before first opening stores with the new binary. Older binaries reject
newer schemas; downgrade by restoring a matching pre-upgrade backup and binary,
not by editing `user_version`. Historical v6 statements/measurements in
[Storage](storage.md) describe that earlier rollout, not the current schema
ceiling; see also [Durable recovery](durable-recovery.md).

For a coordinated backup, stop writers and preserve the server state root,
including each session directory's `state.sqlite3` and any SQLite sidecars,
the store's **`attachments/`** bytes referenced by `FileAttachment` (for example,
`owner-sessions/attachments/`, not a web remote directory), and the matching
web authentication journal (`web/auth.json`, which contains device registrations)
and other journals. References use the existing absolute-path convention. An
online SQLite backup captures committed database content,
not those separate attachment/auth files. Saved references are not a substitute
for attachment bytes; do not copy just the database or assume attachment paths
can be moved to a different root without validation.

On the laptop protect `remote.toml`, its credential and `.grant` fingerprint,
plus `~/.kodrik/remote/` receipt state (`receipts.sqlite3`, sidecars and identity
marker/lock). The receipt journal is not a second conversation store. Losing it
must not be “fixed” by silently clearing unknown operation evidence. Credential
backups are sensitive: revoke a lost device instead of sharing its token.
Verify a restore with a compatible binary and reconcile effects that happened
after the backup before authorizing new work.

## Deployment verification status

The behavior above describes the implementation, not a claim of production
validation. A real two-machine run with the owner's Telegram and Web Chat,
network loss, sleep/resume and SIGKILL has **not been manually verified** for
this documentation update. Loopback/fake-service fixtures cannot establish
platform containment, actual sleep scheduling or a real deployment's proxy and
Telegram behavior. Run that manual acceptance scenario with explicit permission
before relying on it for unattended or consequential work.
