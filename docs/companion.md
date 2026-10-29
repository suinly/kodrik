# Companion

Kodrik is a general conversational companion: programming is one ability alongside
discussing books, science, everyday life and ideas. Its development consists of
persistent, inspectable notes and reusable procedures, not model weight training.

Unrelated topics can automatically start a new session through the configurable
[System One decision provider](sessions.md).

## Personality and continuity

`~/.kodrik/SOUL.md` is created from the bundled personality on first use. Edit it
to change the voice and conversational principles; existing files are never
overwritten. Each session freezes personality, memory, learned facts, active
interests and the skills catalog. Start `/new` to load changes.

The `session_search` tool retrieves actual earlier user/assistant messages, with
session IDs and message indexes. It supports search and browsing a named session.
Successful one-shot CLI conversations are also saved. Tool outputs and image
bytes are excluded from search excerpts. See [Storage](storage.md#session-contents)
for history search limits.

## Learning

After a successful substantive turn, a separate model review examines a bounded
text excerpt. Short acknowledgements are skipped. It may propose up to three
durable facts, reusable procedures or interests. Every accepted proposal retains
the originating session, message indexes, exact supporting quotes and timestamp.
Facts and interests require user evidence; procedures also require an explicitly
successful tool result. These checks establish provenance; the model still makes
the semantic judgment about what is worth retaining.

The journal is `~/.kodrik/companion.json`. Automatic facts are stored there,
separately from the owner-editable `memories/USER.md` and `memories/MEMORY.md`.
Learned procedures use descriptive names such as
`~/.kodrik/skills/deploy-docker-compose/SKILL.md`. The reviewer first compares
knowledge with the installed skill catalog by meaning, then can request the texts
of relevant skills. Built-in and manually created skills are included as read-only
references so existing procedures can be reused instead of duplicated.
The final decision explicitly creates a new procedure, updates a learned skill by
its lesson ID, or skips saving. Updates require inspecting the selected skill and
returning its complete revised steps; they keep its ID, name and path and record
each revision's source. An occupied name never implicitly selects an update or
creates a numbered duplicate. Owner edits and skills outside the learning journal
are never overwritten. Legacy `learned-<uuid>` skills can also be inspected,
updated by ID and forgotten under their original paths.
Pending skill creation or updates are retried on a later successful review.
Limits are 20 lessons / 6000 characters and 10 interests /
4000 characters; full stores reject additions rather than silently evicting data.

Ask naturally:

- “What have you learned from our conversations? Show the sources.”
- “Forget this learned fact.”
- “Remember my interest in city history and why cities developed along rivers.”
- “Pause this interest.”

The `companion` tool lists the journal, manages interests and forgets lessons by
ID. Forgetting a procedure deletes its generated skill only if its content has
not been edited by the owner. Removal does not erase historical messages or old
session snapshots. Use `/new` after forgetting a fact.

Only one review runs at a time; turns arriving while it is busy are skipped.
Reviews make at most two model calls sharing a 30-second timeout, with at most
24 excerpted messages / 10000 text characters and 2048 output tokens per call.
One inspection round may read up to three catalogued `SKILL.md` files, totalling
at most 32000 bytes. No shell commands or skill scripts run during review.
Graceful CLI/TUI/gateway shutdown waits for the
current review; abrupt termination can lose it. Failures are logged, not shown as
a failure of the already completed conversation.

## Interests and daily initiative

While the gateway runs, Kodrik can explore an active interest and send one useful
finding or follow-up question to the owner's or an explicitly configured private Telegram chat. It may choose
silence. Default scheduling is after noon in Moscow, following at least ten
minutes without gateway messages, with quiet hours from 22:00 to 09:00. New
gateway activity cancels ongoing research. The daily claim is persisted before
research, so restarts, cancellation and failures cannot trigger same-day retries.
This means at most one attempt per day, not a guaranteed daily message.

Research receives the current time in `companion.timezone` before every model
call and uses only history search, Wikipedia and optional read-only
[web research](web-research.md) tools; it has no shell or file-editing tools.
`encyclopedia_search` supports Russian/English Wikipedia; configured `web_search`
and `web_read` extend research to public websites. Public topic queries and URLs
must omit private data.
Research is limited to 90 seconds, four model calls, 32000 input characters per
call and 2048 output tokens per call, without provider retries. New findings need
a nonempty evidence tool result. Exact repetition of the previous proactive
message is suppressed; the prompt also asks the model to avoid repeated ideas.
Delivered messages are appended to the normal conversation history.
If an interest's findings or the total interests budget is full, the new finding
is skipped without changing existing notes; a valid proactive message can still
be delivered. Storage errors remain errors and are logged.

Configure the recipient, schedule and enable/disable switches in the
[companion configuration](configuration.md#companion). Defaults work without a
new section when the recipient is unambiguous. Disabling automatic learning or
initiative leaves foreground tools available. Learning and research use the
`primary` provider, independently of `system_one`. Without active interests, no
proactive research runs.

## Agent wakeups

The `wakeups` tool schedules the agent's own one-shot follow-up instructions,
including on its initiative. It supports create, list, reschedule and cancel,
scoped to the source conversation in Telegram, web, CLI/TUI or webhook runs. Only
pending execution may be cancelled or rescheduled. The application binds the
source chat and session; the model cannot
redirect tasks. Scheduling grants no additional action permissions.

The application checks on startup and every five seconds, independently of Telegram.
Web, CLI/TUI and webhook results are atomically appended to the original session
with a delivery receipt, preventing duplicates on restart. Telegram results go
to the source chat. Quiet hours apply to both. A busy conversation defers
execution. Pending tasks survive restarts and `/new`. Each attempt runs the
ordinary agent with all its currently available tools, personality, memory and
skills in an internal `companion:wakeup:<id>` session, excluded from ordinary
history search/listing. It receives the original instruction, deadline and source
and current session IDs so it can check whether the work remains relevant.
Follow-up wakeups keep the searchable original conversation as their source.

Each execution is limited to 16 model attempts (including provider retries) and
five minutes from its original claim; downtime consumes this deadline. Intermediate text,
tool results and plans stay internal. The agent may finish silently, send a useful
message, or schedule another one-shot follow-up. Task tools may run during quiet
hours, but optional messages wait until `companion.quiet_start`/`quiet_end` allow
delivery in `companion.timezone`. The saved message is delivered without rerunning
task tools. Daily interest-research limits do not suppress wakeups.

List includes pending, running, ready-for-delivery, failed and uncertain attempts
with diagnostics. Completed/cancelled tasks remain in the bounded journal.
New wakeups persist a stable `wakeup:<id>` submission and original execution
context before admission. Restart reconciles this claim with SQLite, resumes safe
checkpoints, or reuses a completed result without generating it again. Tool-created
wakeups deduplicate creation by run/call identity; the mixed wakeup tool remains
non-replayable. Legacy interrupted execution, unknown tool outcomes, model/result
failures and exhausted budgets become `uncertain` and are not automatically retried.
Interrupted sends also become uncertain. Confirmed temporary send refusals retry
only the saved message; permanent refusals become failed. Revoked chats and
missing interfaces remain pending/ready with a diagnostic and are never redirected.

Only one wakeup scheduler owns the data directory. Other processes wait and
check every five seconds to take over after the owner exits. Storage failures stop
the scheduler and are logged. Execution is serial, so a long attempt may delay other tasks by up
to five minutes. Telegram delivery and local persistence are not a transaction;
exactly-once delivery is not guaranteed. Recurring schedules are unsupported.
Scheduling in a one-shot CLI command does not keep that process running; use the
gateway for unattended execution. User-facing Apple Reminders use the installed
`remindctl` skill.

## Legacy reminders

The former `reminders` tool is no longer registered. Existing `reminders.json`
records are preserved and pending reminders still send their original text without
a model call. They are never converted into instructions for the agent.

Gateway checks due reminders immediately on startup and every five seconds.
The gateway must be running to deliver; pending reminders missed while offline
are sent after startup, oldest first, with `⏱ +Ns` indicating delay when it
exceeds five seconds. Quiet hours, interest availability, inactivity and the
daily initiative limit do not suppress reminders. A busy conversation is
deferred until it becomes free. Sends are sequential, with a 15-second timeout,
so network delays can also postpone other reminders. Delivery goes into the
chat's current session even after `/new`, without a model call.

Confirmed temporary Telegram refusals retry after at least 60 seconds or a
larger `retry_after`. Permanent refusals become `failed`. A timeout, lost
connection, malformed response or interrupted in-flight send becomes
`uncertain`: delivery may have occurred, and no automatic retry risks a duplicate.
The list shows these states and the last error; create a replacement only if
you decide another delivery is needed. Missing interfaces or invalid recipients
remain pending with a diagnostic; messages are not redirected.

Only one legacy reminder scheduler owns the data directory. Additional gateways skip
starting one; they do not take over automatically if the owner exits. Restart
the remaining gateway to acquire ownership. Storage or session-loading errors
stop the scheduler and are logged; fix the storage problem and restart the
gateway. Successful delivery is marked before saving conversation history,
so a subsequent history error cannot resend the message. Telegram delivery and
disk persistence are not a transaction; exactly-once delivery is not guaranteed.

## Daily initiative persistence

`proactive.json` holds the last claimed local date and last delivered message.
Research transcripts are saved as internal `companion:proactive:*` sessions and
excluded from conversation search/listing. Delivery and local history persistence
are not one transaction: a disk error after delivery can leave an unsaved message;
there is no automatic retry that risks a duplicate. Companion state and learned skills are separate for each chat. Background
learning and wakeups use that chat's runner and tools, including Bashkit for
all groups. Paths in this guide are relative to the current
[space directory](storage.md#chat-spaces).

See also: [Explicit memory](memory.md), [Skills](skills.md), [Storage and backups](storage.md).
