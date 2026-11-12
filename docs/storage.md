# Storage and backups

Server and standalone runtime state lives under `~/.kodrik/`, independently of
the current working directory. Remote TUI credentials/configuration and its local
receipt journal live on the laptop; see the file map and
[remote backups](#remote-state-and-backups). The repository-local `data/`
directory is not used by the runtime.

## Chat spaces

Personal state lives in `users/<positive-telegram-user-id>/` under `~/.kodrik/`,
shared by Telegram and web. Historical group data remains in separate
`spaces/telegram/<interface-hex>/groups/<negative-id>/` directories;
`interface-hex` encodes the configured gateway name's UTF-8 bytes. Different
users and groups never share a space. CLI/TUI use the configured owner's
space, or the legacy root when no owner is configured.

Memory, skills, sessions, datasets, attachments, `SOUL.md`, companion state and scheduled
jobs in the table below are relative to that space. Configuration, web
authentication and published pages remain at the global root.
Web chat uses the authenticated user's personal session store and permissions.
The configured owner shares that store with CLI/TUI; other users retain isolated tools.
Model/tool action logs live in each space; Telegram transport delivery logs use
the owner/local space. Isolated chats cannot publish pages or read the global files through their tools.

Isolated shell, patch and image tools share a persistent virtual disk in
`sandbox.json`, guarded by `sandbox.lock`. The snapshot also tracks imported
skill paths, so deletion on the host removes their virtual copies on the next
input. `/workspace` is its working directory;
`/skills` contains its skills and `/attachments` contains imported documents from
that chat. There are no host mounts or network commands. Files persist between
calls and restarts; shell variables and working-directory changes do not. The
virtual disk is limited to 50 MB total, 20 MB per file, 10,000 files and 2,000
directories. A corrupt snapshot fails closed without using the native shell.

When `[owner]` is first configured, Kodrik copies legacy memory, skills, sessions,
attachments, personality and companion state into the owner's space. Only jobs
addressed to that owner's configured private chat are copied. Groups start with
fresh state; old group jobs and histories are never resumed there. The original
files remain untouched. Migration runs under `spaces.lock` and completes only after
`.personal-migration-complete` is written in the personal space. The configured
owner also gets `.owner-migration-complete`, so promoting an existing personal
user still imports local CLI/TUI history once. Legacy Telegram
personal directories and the owner's old `web/chat-sessions` archive are merged
once, preserving titles, pins, branches, selection and absolute attachment paths.
An old `.owner-migration-complete` marker makes the old Telegram owner space
authoritative over its stale root backup. `.web-chat-migration-complete` at the
global root prevents importing the old web archive again if owner configuration changes.
The global `.owner-root-migration-complete` records the first root claimant;
changing the configured owner never assigns retained root history or the old web
archive to another user. Upgrades recognize existing owner markers across all
personal spaces. Conflicting prior owner markers require reconciliation.
Source directories remain as backups. An existing space
does not suppress migration: missing files are added, identical files retained,
and unrelated space files preserved. Conflicting files stop startup before any
transfer; back up both copies, reconcile the reported path, then restart. A
partial transfer can be retried. Stop old processes before upgrading to avoid
concurrent legacy writes. Back up the whole root, including legacy files and
`spaces/`, `users/` and the old web attachments.

An initialized dataset database with no datasets, records or write receipts is
treated as empty. Empty sources are ignored; an empty destination can receive a
validated snapshot. Different nonempty dataset databases still stop migration.

If either session directory already uses SQLite, owner migration builds a merged
database in staging using SQLite's online backup API, including committed WAL
pages. Conflicting session payloads stop migration before replacement. Active
selectors use the current personal store first, then legacy personal stores,
then the old shared root. JSON archives already imported into SQLite remain in
their original locations as backups and do not override current SQLite state.
Completed personal migrations are not replayed when that user becomes owner.
Open Kodrik stores hold shared `store.lock` locks; migration needs
exclusive ownership of both stores, so stop CLI/TUI/web/gateway processes first.
The source database remains intact. An interrupted first JSON import is retried,
not mistaken for an empty, successfully migrated database.

## File map

| Path under `~/.kodrik/` | Contents |
| --- | --- |
| `config.toml` | Provider credentials, gateways and behavior settings |
| `SOUL.md` | Owner-editable personality; initialized once, limited to 32768 bytes |
| `memories/USER.md` | Explicit profile and preferences; 1375 Unicode characters |
| `memories/MEMORY.md` | Explicit environment facts and conventions; 2200 Unicode characters |
| `sessions/state.sqlite3` | Session JSON payloads, active-session pointers, durable run/tool state, remote bindings/operations, selected-text attachment metadata and lightweight chat listing metadata; current schema v16 |
| `sessions/attachments/` | Immutable attachment bytes referenced by saved `FileAttachment` paths, including explicitly saved remote text; keep referenced source directories after migration |
| `datasets/state.sqlite3` | Typed record definitions, values, revisions and idempotent write receipts; see [Datasets](datasets.md) |
| `datasets/state.sqlite3-wal`, `datasets/state.sqlite3-shm` | Dataset SQLite sidecars; include committed WAL data in backups |
| `datasets/store.lock`, `datasets/init.lock` | Dataset store ownership during migration and serialized initialization |
| `sessions/state.sqlite3-wal`, `sessions/state.sqlite3-shm` | SQLite WAL and shared-memory sidecars; do not copy a live main database alone |
| `sessions/store.lock` | Shared store ownership; exclusive during owner migration |
| `sessions/init.lock` | Serializes first-open schema/WAL setup and legacy import |
| `sessions/<id>.json`, `sessions/active/<conversation-id>.json` | Preserved legacy import sources; no longer updated after migration |
| `web/chat-sessions/state.sqlite3` | Preserved legacy owner web archive; imported once into personal sessions |
| `logs/actions/YYYY-MM-DD.jsonl` | Private append-only runtime action journal, split by UTC date |
| `skills/<name>/SKILL.md` | User skills and their supporting files |
| `skills/learned-<uuid>/SKILL.md` | Procedures produced by automatic learning |
| `skills/.system/` | Generated cache of embedded skills; do not edit |
| `pages/drafts/<slug>/` | Agent-editable source files and records for a page |
| `pages/releases/<slug>/<uuid>/` | Published page versions; old releases are retained |
| `pages/releases/<slug>/<uuid>/document.json` | Native MDC definition, named source queries and frozen published snapshot; browser responses omit query definitions |
| `pages/manifests/<slug>.json` | Current release and public/private access mode; not served |
| `pages/locks/<slug>.lock` | Publication transaction file lock |
| `web/auth.json`, `web/auth.lock` | Owner password hash, hashed session tokens/invitations/device tokens, device registrations/pairing requests, per-user display names, pending Telegram approvals, discovered public bot names and auth lock; journal mode 0600 |
| `remote.toml`, `remote/device.json`, `remote/device.grant` | Laptop defaults: provider-free remote configuration, raw device credential (0600) and manifest grant fingerprint; config may override credential path |
| `remote/receipts.sqlite3`, `remote/receipts.sqlite3-wal`, `remote/receipts.sqlite3-shm` | Laptop operation receipts and ACK evidence, separate from server conversation history |
| `remote/journal-created` | Laptop receipt identity marker and initialization lock; preserve with the receipt database |
| `companion.json` | Learned lessons, interests and their evidence |
| `proactive.json` | Last daily attempt date and last delivered initiative message |
| `wakeups.json` | Agent instructions, bound source chat/session, UTC deadline, execution/delivery state, saved message and last error; up to 8 MiB, no automatic eviction |
| `wakeups.lock`, `wakeups-scheduler.lock` | Wakeup store updates and single-scheduler ownership |
| `reminders.json` | Preserved legacy reminders: source chat, UTC deadline, delivery state and last error; up to 8 MiB, no automatic eviction |
| `reminders.lock`, `reminders-scheduler.lock` | Reminder store updates and single-scheduler ownership |
| `companion.lock`, `proactive.lock` | File-lock coordination for the corresponding stores |

Dataset owner migration snapshots committed WAL data through SQLite backup and
refuses active writers or divergent source/destination records. It preserves
the original root store. Dataset files and sidecars are private (`0600`) inside
a private directory (`0700`); writes and receipts are transactional. There is
no automatic record or receipt retention.

Memory and skill installation also use lock files. A lock file remaining on disk
does not mean a process still holds its OS lock.

Telegram approval requests store hashes of the public request ID and independent
browser secret, the bound interface/user, decision and expiry. At most 64 pending
requests are retained; they expire after five minutes and are redeemed once.
The public bot link and locally generated QR contain only the request ID.
Changing the web password revokes pending requests as well as existing sessions.

Remote device pairing is separate from Telegram web-login approval. Device
registrations retain token hashes, not raw credentials; pending pairings retain
code/polling-secret hashes and expire after five minutes. Device credentials
expire after 30 days. Restore the matching auth journal with server state and
revoke lost devices through the owner controls rather than sharing credential
backups. See [configuration and pairing](remote-tui.md#configure-and-pair).

## Session contents

Sessions save messages and snapshots of personality, memory, skills and companion
context. Image data URLs are stored inline, so images can make session payloads
large. `last_message_at` is an optional UTC timestamp serialized as RFC 3339
for the last user/assistant message. Tool activity does not update it; it no
longer controls session routing.

The database uses WAL with `synchronous=FULL`, foreign keys and a five-second
busy timeout. Session directories are mode `0700`; database and sidecars are
mode `0600`. New-session creation and active-session selection are transactional.
Deleting a completed session removes its active pointers and run/tool records;
the next load can create a fresh conversation. Sessions with unfinished runs
cannot be deleted. SQLite work runs on blocking workers, outside the async
executor. Ordinary foreground runs save input, tool intents, results and terminal
history at transactional checkpoints; see [durable execution](sessions.md#durable-execution-checkpoints).
Interrupted runs are recovered by the matching client on startup, subject to
saved permissions, replay policy, cancellation and crash limits. A remote
`EnvironmentPaused` run is not automatically resumed by reattach/recovery;
renewed device credentials or a restored binding do not restore local authority.

History search reads user/assistant text from the newest 1000 database sessions within a
64 MiB payload budget, skipping individual sessions over 16 MiB. It excludes tool
outputs, image bytes and internal proactive/wakeup sessions. This is a bounded
history search over stored payloads, not an exhaustive text index. Response fields
`scanned_files` and `skipped_files` keep their existing names for compatibility
and now count database session records. Corrupt records are skipped by search;
ordinary session loading reports the error.

## First SQLite startup

Opening a session directory imports its legacy JSON sessions and `active/`
mapping files in one transaction. The session ID comes from the JSON payload,
not the filename. Equal duplicate payloads are accepted; conflicting payloads,
malformed JSON, symlinks and pointers to missing sessions stop the import with
the offending path. All imported rows and the completion marker commit together.
After correcting the reported legacy data, restart to retry the import.

Original JSON files remain byte-for-byte unchanged. Once the import marker is
committed, SQLite is the only source of truth: changing stale JSON does not edit
current history and deleting a database session does not resurrect its old JSON.
Unknown newer database schema versions stop startup without overwriting data.
Run and tool-execution tables hold versioned checkpoint payloads. History, run
revision and tool outcome commit atomically; stale revisions are rejected. The
submission table now stores canonical input, sequence and status/run/session
binding. Schema v3 adds those indexed state columns and `inbox_controls` for
paused queues; initial submission binding commits with the first run checkpoint.
Agent-directed handoffs persist source/target histories, run binding and active
selection together.
Schema v2
adds `run_controls` cancellation tombstones, including cancellation before the
first run checkpoint. Schema v4 adds `deliveries` with JSON receipts for final and explicit
Telegram replies. Schema v5 adds a separate `runs.live` partial payload and a transactional view
revision in schema_meta, advanced by triggers on run/history/inbox/control/delivery/compaction
changes. Partial flushes do not rewrite Session JSON or checkpoint revisions.
The current schema ceiling is **v16**. Opening v1–v15 upgrades transactionally;
run payload version remains 1 with backwards-compatible optional fields. Older
binaries reject a newer schema. Schema v6 introduced compaction jobs; v7–v8
added web-push state/presence; v9–v11 added command audit records/settings/modes;
v12 adds `remote_bindings` and `remote_operations`; v13 adds
`remote_tool_attachments` keyed by run/call. v14 adds chat activity/read markers,
v15 permits active draft selectors, and v16 caches lightweight sidebar metadata
in `web_chat_activity`. The v16 upgrade reads existing histories once to backfill
the cache, preserving read markers and full session payloads. Normal sidebar
refreshes no longer read those histories; text search still scans saved history.
Historical v6 rollout measurements
in [Durable recovery](durable-recovery.md) describe that earlier binary, not the
current compatibility ceiling.

Owner migration accepts v1–v16 and carries compaction jobs, submissions,
cancellation controls, paused inbox state, delivery receipts, live payloads,
web-push/audit state and available remote tables, preserving a revision above
both source clocks. Earlier audit settings are adapted without changing source
databases. Remote attachment references are preserved, not relocated; retain
their source files/directories. Delivery receipts survive history deletion;
remote operation/attachment metadata follows its run's foreign-key lifetime.
Run payloads also retain the active-attempt marker, consecutive recovery count,
interrupted phase for parking, one-shot manual retry allowance, confirmed history
length for run snapshots, model attempts,
provider retry count, `next_retry_at`, and optional
wakeup model/deadline limits. Ordinary durable generation persists each attempt
and backoff before waiting; the provider performs one request per attempt.
`Retry-After` survives restart. Retrying a model request may incur another charge.
Bounded research and custom session-only consumers retain their previous wrapper.

`sessions/execution-locks/` holds advisory locks named by SHA-256 hashes of their
namespace and identifier. Locks cover conversations, bound sessions, deliveries
and short recovery scans; process exit releases them. Lock files contain no session data.
See [recovery and stopping](sessions.md#recovery-and-stopping) for restart rules.

## Action journal

The CLI and gateway automatically journal model runs and tool execution, including
wakeups, proactive research and learning. Telegram send, edit, draft and delete
requests record their own delivery attempts and outcomes. This journal does not
depend on UI progress or on the final session save.

Each line is a JSON object with `version`, UTC `timestamp`, `event`, `status`,
`session_id`, `run_id`, `action_id`, optional `call_id` and `source_id`, `trigger`,
`name`, `duration_ms` and `details`. Events are `run_started`, `run_finished`,
`tool_started`, `tool_finished`, `delivery_started` and `delivery_finished`.
`action_id` pairs one start with its finish; `run_id` links a foreground run and
its Telegram operations, or proactive research and delivery. Wakeup execution
and later delivery have separate run IDs and share the wakeup's `source_id`.
Legacy reminder deliveries also carry their reminder ID as `source_id`.
Triggers are `message`, `one_shot`, `wakeup`, `proactive`, `learning`, `reminder`
and `delivery` (operations outside a model run).

Run/tool finishes use `ok` or `failed`. Shell outcomes include `exit_code`,
`timed_out` and retained stdout/stderr byte counts; nonzero exits, signals and
timeouts count as failures even if the tool itself returned successfully.
An overall run can succeed after handling a failed tool. An audit run finish describes
model execution; the SQLite terminal checkpoint and delivery receipt are the
authority for durable completion and delivery.
Delivery outcomes use `sent` for a confirmed API operation, `refused` for an
explicit API rejection, `unchanged` for an idempotent edit, or `unknown` for an
unconfirmed outcome. A delivery `name` identifies the API method, so a confirmed
draft, edit or deletion is distinguishable from a new message.
Explicit Telegram refusals include numeric `error_code` and `retry_after`
seconds, without storing the API description. Foreground final replies wait for
an explicit 429 `retry_after` before retrying; ambiguous network errors are not
retried by that delivery helper. Intermediate preview updates are coalesced to
two-second intervals and stop after a failure for the remainder of that run.

Starts are appended and synced before execution. Writers lock the daily file,
and filesystem I/O runs on blocking workers. If a write fails, subsequent
journaled actions in that process are blocked, including autonomous tools and
deliveries. A result-write failure stops the tool loop; a confirmed tool result
is saved with the SQLite run checkpoint before attempting the audit finish
in ordinary foreground runs. Session-only/background wrappers retain the result
in memory for their existing completion save. After a Telegram
API operation, its actual result is preserved even if journaling fails: a journal
failure must never turn a confirmed delivery into an automatic resend.

A start without a finish means **unknown**, not failed or safe to retry. This can
follow cancellation, timeout, a process crash or a failed outcome write. On a
partially written final line in the current daily file, the journal refuses further appends rather than
silently discarding evidence. Stop writers, preserve the original file, investigate
the affected action and repair the incomplete tail before restarting. Resolve
disk/permission failures before restarting as well. Audit lines are not replayed;
SQLite recovery separately applies the saved/current tool replay policies.

Journal files are mode `0600`; new directories are mode `0700`. Raw prompts,
commands, arguments, outputs, message bodies and error text are omitted. Details
contain only selected numeric/boolean metadata, such as argument/request size,
chat/message IDs and the number of learned entries. To investigate command text,
use the corresponding session's existing tool-call history when available. The
journal is not tamper-proof against a process with the same filesystem permissions,
and it records tool boundaries rather than every subprocess/file operation inside
a shell command. There is no automatic retention or size-based eviction.

Inspect recent events or one run without any database (replace the legacy
`~/.kodrik` prefix with the current space directory when configured):

```sh
jq -c '{timestamp,event,trigger,name,status,run_id}' ~/.kodrik/logs/actions/*.jsonl
jq -c --arg run 'RUN_UUID' 'select(.run_id == $run)' ~/.kodrik/logs/actions/*.jsonl
jq -s 'group_by(.action_id)[] | select(length == 1 and .[0].status == "started") | .[0]' ~/.kodrik/logs/actions/*.jsonl
```

## Back up and restore

1. Stop all Kodrik processes that can write state, including the gateway.
2. Copy the entire `~/.kodrik/` directory to your backup location.
3. Restore it into the intended user's home directory before starting Kodrik.

After an unclean stop, retain SQLite `-wal` and `-shm` sidecars alongside their
main database; committed data may still be in WAL. For a live database-only
backup, use SQLite's online backup API (or the sqlite3 shell's `.backup` command),
not an ordinary copy of `state.sqlite3`. A database-only backup does not contain
attachments, memory, configuration or the other state directories.

Do not run an older JSON-only binary against migrated state: it would read stale
files and create divergent history. Roll back using a complete compatible
pre-upgrade backup; deleting SQLite to fall back to old JSON loses newer work.

The backup includes API credentials, bot tokens, private messages and image
bytes. Keep it private. No automatic backup or retention policy is provided.

### Remote state and backups

Remote mode keeps history, memory, run intents, remote operations and attachment
metadata in the server owner's session store; it does not synchronize laptop
sessions into that store. Remote bindings saved in SQLite are evidence, not a
live lease. A restore or reattach cannot replay an uncertain local effect.

Explicit `read_file` calls with `save_attachment = true` can persist nonempty
UTF-8 text (at most 1 MiB) as immutable `FileAttachment` bytes under the session
store's `attachments/`, with run/call references in `remote_tool_attachments`.
Ordinary reads do not create downloadable files. Images remain inline data URLs
in the existing message/output format. There is no separate web-remote
attachment root. Paths use the existing absolute-path convention: a database
backup alone does not contain those files, and moving a restore to a different
root requires validating references. Owner migration retains source attachment
files rather than rewriting all references or garbage-collecting them.

Stop all writers for a coordinated server backup and preserve the entire state
root: session databases/sidecars, referenced attachment bytes (including legacy
source directories), `web/auth.json` and the other journals. An online SQLite
backup captures committed WAL data, not separate attachment/auth files.

On the laptop preserve `remote.toml`, its configured credential and `.grant`
fingerprint, and `~/.kodrik/remote/` with the receipt database/sidecars and
`journal-created` identity marker. Custom config/credential paths may be outside
`~/.kodrik/`; include them explicitly. The journal is private local execution
evidence, not a second conversation store. Acknowledged receipts older than
seven days are pruned on open; unacknowledged evidence is not silently evicted.
Losing/replacing the journal while its marker remains fails closed: reconcile
unknown outcomes instead of deleting the marker to bypass the check. Reconcile
external effects since a backup before authorizing new work. See
[remote attachments and backups](remote-tui.md#attachments-migration-and-backups).

## Editing and forgetting

Prefer the memory and companion tools for changes. Before manual edits, stop
other writers and preserve the documented formats. Start `/new` to refresh
snapshots after changing personality, memory or skills.

Forgetting a memory or lesson does not erase earlier copies from session history
or snapshots. Kodrik has no single operation that purges a fact from every saved
conversation and backup. See [Memory](memory.md) and [Companion](companion.md).


Schema v6 adds `compaction_jobs`: one current job per session with source snapshot,
identity, run owner, status, candidate and separate usage counters. The table is
private and included in owner migration/backup. An OS lock under execution-locks
limits summary computation to one per session across processes. Jobs never mutate
live Session objects; candidate application updates session and job status in one
transaction. Session generation distinguishes a reset from an appended transcript.

## Verify a restore before resuming work

Use a separate destination, keep the source backup untouched, and check the
restored database before starting any agent worker:

```sh
sqlite3 /path/to/restored/state.sqlite3 'PRAGMA integrity_check; PRAGMA user_version;'
```

Integrity should report `ok`; the binary must support the recorded schema
(currently v13 for session stores). This only verifies SQLite structure, not
whether an external action happened. Include attachment originals and the matching wakeup/delivery
state. Before resuming a restored backup, inspect external effects that could
have occurred after that backup; rollback can otherwise make a past action look
unfinished again.

For an online database snapshot, use a new private destination:

```sh
sqlite3 /path/to/live/state.sqlite3 ".backup '/path/to/backup/state.sqlite3'"
```

This snapshot includes committed WAL content, but is not an atomic backup of all
Kodrik JSON journals, attachments and databases together. Stop all writers for a
coordinated whole-root backup. Never delete a corrupt/future-schema database to
make startup succeed and never use old JSON files as current history. Keep the
failed database and sidecars for investigation; restore a known compatible copy
with its matching binary. See [tested storage cases](durable-recovery.md#storage-checks).

## Invitation access

`web/auth.json` also stores hashed single-use invitations, their labels and
expiry times, admitted Telegram IDs, and the configured owner ID. Treat this
private file as part of the full-state backup; removing it revokes invitations
and membership along with sessions. No raw invitation URLs are retained.
Groups are disabled. Private guest chats for the owner and invited users are stored
under `spaces/telegram/<hex-interface>/guest-chats/<lower-user-id>-<higher-user-id>/`,
separately from personal data and other participant pairs. Both directions share
one session, memory, datasets and virtual filesystem. Old per-caller guest
directories remain untouched and are not automatically imported into the shared
space. Guest chats use sandboxed tools and
are reopened on the next authorized guest request, without background schedulers.
Existing uninvited user directories
remain on disk but their applications and background jobs are not recovered.
Use `kodrik invites list` and the revoke commands to manage access; do not edit
the journal while the gateway is running.
