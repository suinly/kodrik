# Automatic sessions and System One

Kodrik checks whether a new turn belongs to the active conversation only after
at least 30 minutes since the last user or assistant message, before the main
agent answers. During an active conversation there is no session-routing call;
[group participation](usage.md#telegram) is decided separately for each message.
A clearly unrelated, self-contained topic starts a new
session. Follow-ups, corrections, answers, acknowledgements and related topics
continue the current one. Ambiguous decisions also keep the current session.

## System One decisions

The decision uses a reusable `SystemOne::choose` service: JSON state plus a
question and named criteria, returning one validated choice. This is a Jev-like
request design implemented through the configured generative provider, not a
native Jev integration or a calibrated probability model. It does not generate
explanations, use tools, or take actions. Each consumer owns its fallback policy.

Configure a separate fast model through [`provider.system_one`](configuration.md#providers).
If absent, Kodrik uses `primary` with separate decision limits. See
[session configuration](configuration.md#sessions) for the switching controls.

## Idle time and persistence

`idle_minutes` is a nonnegative integer; `0` checks every eligible turn. The
last user/assistant message time is saved in the SQLite session payload and survives restarts.
Tool calls and tool results do not reset it. Replies from the agent, including
proactive messages, do reset it. A long pause permits classification; it never
forces a new session by itself. Future timestamps (clock rollback) suppress
classification until the configured pause has elapsed.

Legacy sessions without a timestamp conservatively continue on the first new
turn. The next user/assistant message establishes the timestamp. No timestamp is
guessed from file modification time.

## Context and failure handling

The router sends **all user and assistant text in chronological order**, followed
by the new message. It never truncates history by message count or character
budget. Tool calls, tool outputs and image payloads are excluded; image captions
remain. Personality, skills and memory snapshots are not included in the decision
request. Empty sessions and incoming messages without text bypass classification.

The model chooses `continue`, `new` or `uncertain`. One request has a 15-second
provider deadline, at most 128 output tokens and no retries. The same deadline
applies to System One decisions about group participation. The response must be
exactly a JSON object such as `{"choice":"new"}`; unknown values, extra fields,
invalid JSON, provider failures and timeouts retain the current session and log
the error. Timeout errors include the deadline (`System One decision timed out
after 15s`). An oversized history is not silently shortened; a context-limit error
also retains the session. These limits bound the extra decision call, not the
main agent's answer. No numeric confidence is invented by the model.

## Switching and cancellation

Switching preserves the previous session and uses the existing active-session
mapping. Fresh sessions reload personality, memory, interests and skills. TUI
and Telegram show a brief switch notification. The TUI clears the previous
history while preserving the new message, queued corrections and the input draft.
Personal sessions are shared per user across Telegram and web; CLI/TUI always uses
the configured owner. Each interface keeps its own active-session selection.
Previous sessions remain available through `/sessions` in TUI, `/sessions` and
`/session <id>` in private Telegram chats, the web sidebar and history search.
`/new` continues to work manually.

Classification runs only at turn boundaries under the conversation lock.
Messages consumed as steering during an ongoing run stay in that run. Cancellation
while classifying preserves accepted input and corrections without switching.
Once a persistent session transition starts, it is allowed to finish before
handling cancellation. Proactive messages are not classified; they enter the
normal history and are visible when classifying the owner's next reply. One-shot
CLI requests already create their own sessions and need no routing.

## Model evaluation

Automated tests cover routing mechanics and failure handling with local provider
fixtures. Semantic accuracy and latency of a chosen live model should be measured
on representative Russian conversations, especially short replies and gradual
topic changes. A structurally valid decision can still be semantically wrong.

See also: [Manual session commands](usage.md), [Storage and backups](storage.md).

## Automatic context compaction

Before each ordinary agent request (including steps after tool execution), Kodrik
checks the active context against `sessions.compact_threshold_tokens`, default
80,000. Durable sessions start background preparation at threshold minus
`sessions.compact_background_margin_tokens` (default 16,000: trigger 64,000).
A margin of 0 keeps blocking compaction only. A threshold of 0 disables future
automatic compaction; existing summaries
remain active. The last provider-reported
input usage can also trigger compaction; it is saved across restarts.

The pre-request estimate includes instructions, tool schemas and messages. Text
uses UTF-8 bytes / 3 plus message overhead, images use a 4,096-token allowance and
files use their byte size / 3. These are heuristics, especially for PDFs, compressed
documents and different vision models. Configure a threshold with headroom below
your endpoint's actual window. A single oversized input can still exceed that
window; this is not an exact tokenizer. Confirmed context overflow has one limited
recovery attempt, described below.

Old messages become a summary of goals, constraints, decisions, relevant facts,
tool outcomes and pending work. The recent suffix targets one quarter of the
threshold. At least the last message/tool group remains verbatim; parallel tool
calls and their results are never split. Very long tool runs may summarize the
original user request, retaining its goal and constraints in the summary.

The summary is supplied as historical assistant context, not system instructions.
Personality, memory, skills and system instructions keep their existing snapshots.
The full transcript and attachments remain in session storage for history, search,
learning and session routing. Summary plus covered-message offset is persisted;
legacy sessions start uncompacted. Invalid saved offsets fall back to full history.

Tool results in model requests are limited to approximately 10,000 tokens
(30,000 UTF-8 bytes of serialized output). Oversized results retain their beginning
and end with an explicit truncation marker. This applies to ordinary requests and
compaction, including existing sessions; stored results and image attachments are
unchanged. Full command output remains available through `stdout_path`/`stderr_path`
when the command's own capture limit was exceeded.

Compaction uses the primary model without tools or streamed summary output, with
at most 2,048 output tokens and a 60-second deadline per call, without transient
retries. Large histories are summarized in chunks targeting half the configured
threshold, carrying the previous summary forward. A single complete message/tool
group can exceed that target and is still attempted after tool-output truncation.
This adds model calls, cost and latency.
TUI and Telegram display compaction progress. Bounded background/wakeup runs do
not acquire extra compaction calls.

New summary state is applied only after every chunk succeeds and the result is
nonempty, bounded text that reduces the active history. Explicit incomplete/failed
response statuses and provider refusals are rejected. Cancellation or any error
preserves the previous summary and the complete transcript. The current turn
reports an error on failed compaction; it does not silently drop history or send
a known-large context anyway. If the summarizer returns a structured context
overflow, compaction removes the oldest complete message/tool group from its
temporary request and retries, preserving call/result pairs. The final history
group is never discarded; an overflow there fails compaction. Other provider
errors are not retried this way. Each attempt is accounted for separately, with
unknown usage for rejected attempts. Truncated output and omitted old groups may
lose details from the summary; the original history remains in storage.

### One recovery attempt after context overflow

A structured provider `context_length_exceeded` error bypasses generic retries.
An ordinary run can force compaction below the configured threshold and retry
once, only after a nonempty summary reduces the active history. The complete
transcript remains stored. An existing compatible background job is reused.

The retry flag is persisted per model step before summary generation. Recovery
also remembers the previous compacted prefix, so reopening after summary apply
but before the next checkpoint does not summarize again. The flag resets with a
successful model step; a second overflow fails with a request to reduce input or
start a new session. Process-loss recovery can still repeat an unfinished
read-only model call; it does not grant another overflow-compaction allowance.

A threshold of 0, bounded runs, missing compactable history, a final indivisible
message/tool group that still exceeds the provider window, or a failed/non-reducing
summary stop recovery without another ordinary model request. Stop cancels the pending compaction.
Summary usage remains separate from ordinary model attempts; absent usage is
not estimated as provider-reported usage.

HTTP 400/413 alone, attachment errors, and error-message text do not identify
context overflow. Deterministic HTTP client errors are not generically retried;
408, 429, and server failures retain their retry policy. Compatible providers
must return the structured code to enable overflow recovery. The Responses API
[documents a 400 response when input exceeds the context window](https://developers.openai.com/api/reference/resources/responses/methods/create);
status alone is insufficient for classification.

## Durable execution checkpoints

Ordinary `AgentRunner` conversations (Telegram, TUI and web) and one-shot CLI
requests use SQLite checkpoints. Prepared input and context snapshots are saved
before model generation, including steering before a compaction request. A full
model response and its ordered tool calls are saved before tool execution. Each
tool intent records its final arguments and replay policy before the audit intent
and execution; the actual result, updated history and next phase are committed
in one transaction before any subsequent effect or `ToolFinished` event.

Final history and terminal run state are committed together, after the run audit
finishes and before the caller receives a successful answer. Streaming text is
provisional; token deltas are not durable checkpoints. Telegram delivery remains
separate from run completion. An audit failure after an effect preserves its known
result and stops execution; it does not authorize another attempt.

A failed or unconfirmed commit stops execution without writing the in-memory
history over the last confirmed checkpoint. Run revisions reject stale updates;
retrying the exact same checkpoint is idempotent. Repeated tool results cannot
append a second result to history. Completed sessions can be deleted with their
run records; deletion of a session with an unfinished run is rejected.

Replay defaults to `Never`. Session search, encyclopedia lookup, local image
reading and GET-only web search declare `Safe`. Browser-based `web_read` stays
`Never` because page JavaScript can cause external effects. Shell, patch,
publishing, memory mutations, Telegram, wakeups and mixed-action tools also stay
`Never`.

## Recovery and stopping

On startup each client resumes only its own unfinished runs: `ask` resumes
one-shot CLI runs, TUI resumes `cli:main`, web resumes web conversations, and
Telegram resumes authorized conversations after their space and interface exist.
Recovery keeps the original session binding even after the active session changes.
Missing attachments, changed isolation permissions, unavailable recipients or
legacy records without isolation provenance block recovery with a diagnostic.

A saved model request may be generated again. A tool whose intent has no result
is executed again only when both its saved and current policy are `Safe` and the
tool is still available. Otherwise recovery records an interrupted, unknown
outcome and asks the model to verify external state or request clarification.
This prevents mechanical replay; the model can still decide to request a new
action. Completed runs are never generated again.

OS advisory locks protect both conversation and bound session across processes.
A short store lock serializes recovery scans. SQLite commit workers and sandbox
workers retain execution ownership until they actually stop, even if their async
caller is dropped. Other conversation history writers use the same locks.

User stop writes a persistent cancellation marker before stopping the active run.
On restart, a marked run is finalized as cancelled before new effects; late
non-cancellation commits are rejected. Ctrl-C/process shutdown suspends execution
at the last confirmed checkpoint instead of recording a user cancellation.
TUI Escape and `/stop` are explicit user cancellation. Timeouts remain execution
errors; abrupt process loss leaves the last checkpoint for recovery.

**Recovery controls:** consecutive crashed attempts in one semantic phase park
a run after `sessions.restart_limit` (default 5); 0 disables automatic recovery.
Use [run administration](operations.md#inspecting-and-controlling-durable-runs) to
inspect, retry or cancel it. Telegram completion and
outbound delivery are separate, with persisted delivery receipts described below.
Recovered runs do not yet stream into a reattached UI (stage 07). Wakeups reconcile
safe checkpoints within their original budget; bounded research keeps its existing
execution/save behavior.
Direct `Agent::run`/`run_bounded` wrappers and custom `SessionStore` implementations
without `runtime_store()` remain session-only.


## Persistent inbox

SQLite stores each ordinary input before acknowledging admission. Submissions
have a request ID scoped to their conversation, a monotonic sequence, payload
hash, status and optional run/session binding. A repeated ID and canonical
payload return the original record; changed content or mode is a conflict.
Attachment hashes, names and MIME types participate in identity; random download
paths do not. File contents and parent directory entries are synced before
admission. Missing/corrupt originals block execution instead of silently losing
attachments.

`FollowUp` creates a later run; `Steer` joins the current run at a tool-round
boundary; `RejectIfBusy` rejects before admission. The inbox allows at most 100
queued ordinary inputs per conversation. Control commands remain admissible when
this queue is full. Overflow explicitly rejects only that input; other chats
continue processing. A following request cannot overtake a queued follow-up.
History, applied steering, run state and completion status commit together.
Automatic session routing binds the submission and updates the active pointer
in that same first checkpoint transaction. Already bound requests retain their
original session even if `/new` changes the active pointer.

After failure or cancellation, independent follow-ups remain queued with a
blocking reason. Only genuinely new input resumes them in order; duplicate
transport delivery cannot unblock them. `/stop` cancels the current run and
unapplied steering admitted before that command. Inputs admitted after the command
remain queued and resume processing once it is applied. `/new` waits for cancellation before creating a new session;
its persistent command ID prevents a repeated delivery from switching twice.
Each independent run retains its own reply and cancellation lifecycle. Guest
queries use separate response slots and bypass ordinary group participation.
Other group messages still undergo the participation decision; silently observed
messages commit history and completion without invoking the agent loop.

CLI/TUI generate an ID for each new input. Telegram uses bot/interface, chat,
message identity and event kind (guest queries use their query ID); edited
messages remain unsupported and ignored. Polling advances its offset only after
the gateway confirms durable admission. Attachments rejected before admission,
and pre-command downloads explicitly discarded by `/stop` or `/new`, do not
become accepted work. Startup recovery also finds admitted work with no run yet.
Guest requests with unavailable response slots remain blocked on restart.

Web provides request-status lookup and stable retry IDs; see [Web chat](web-chat.md).
Final Telegram replies create a Pending receipt atomically with run completion.
Explicit `telegram_reply` persists its rich payload before sending and suppresses
an additional final reply. Foreground delivery owns its receipt until preview
handoff; startup and periodic recovery send remaining Pending receipts. Sent
receipts are never resent; orphan Sending receipts become Uncertain. A confirmed
429 persists a retry time measured from the response; `/stop` cancels pending
retries, including refusals arriving after stop. Revoked recipients are skipped.
Receipt storage failures stop further effects. RestartCoordinator still uses its
own confirmation receipts and is notified after confirmed deferred delivery.

These guarantees do not provide exactly-once external effects: a process can die
after Telegram accepted a send but before its confirmation was saved. Such unknown
outcomes require inspection, not automatic resend. Async model backoff releases
its executor thread and database connection, but retains conversation ownership
and its logical web slot until completion or cancellation.


## Durable run snapshots

The internal `run_view`/`subscribe_run` API reads a consistent SQLite snapshot of
run state, confirmed history, successful `update_plan` results, queued inputs,
cancellation controls and delivery receipts. New runs record their confirmed
history boundary, so a completed run's view does not adopt later turns. Legacy
runs without a boundary retain the available saved history.

Observers start with a full snapshot and receive newer revisions. They never
submit work, recover it or cancel it when dropped. Delivery is latest-only: one
snapshot replaces any skipped states, below the 100-frame backlog cap. A 250 ms
revision poll handles writers in other processes and closes snapshot/subscribe
races. Revisions are store-wide and persistent; unrelated activity may trigger a
redundant snapshot. Duplicate or older frames are ignored by the web projection.

Streaming text is coalesced every 250 ms into a separate compact `runs.live`
payload; attempt completion/error flushes its remaining text. A successful provider return
is only Ready; it becomes Committed atomically with the confirmed history. Confirmed history
and tool outcomes still commit at semantic boundaries. A new attempt marks the
previous unfinished partial as interrupted rather than concatenating it. Committed
partial text is omitted from the rendered snapshot in favor of canonical history.
A crash or interrupted worker can lose text not yet flushed; a stalled disk has
no wall-clock persistence guarantee. Non-streaming provider calls keep their
existing request format and expose their result when confirmed.

Generation completion is separate from delivery: a Completed run can still have
a Pending or Uncertain receipt. Legacy progress callbacks are best-effort and do
not block durable work; a saturated old UI can miss transient events. HTTP watch,
reload/reconnect and removing web-disconnect cancellation are stage 08. This
internal observer API does not yet change the existing web disconnect behavior.


### Durable background compaction

A job saves an immutable source snapshot before calling the summary provider.
One computation per session runs without acquiring the conversation lock. Below
the blocking threshold the ordinary model/tool loop continues; at the threshold
it waits for the prepared result. Summary text never streams into the user reply.

At a model-step boundary, one SQLite transaction checks session generation,
previous summary/through, covered-prefix hash, active selection and cancellation,
then applies the candidate while retaining the current appended suffix. An older
candidate cannot overwrite a newer summary. Reset and session selection invalidate
candidates; Stop cancels the run's job, including late results. Full history,
attachments, tool-group boundaries and token estimation remain unchanged.

Jobs and ready candidates survive restart. The next eligible model-step boundary
resumes an unfinished read-only summary or reuses a ready candidate without a new
provider call. A completed run can leave a ready candidate for the next turn;
observers never trigger execution. A stale candidate is discarded and recomputed
when still needed. Bounded wakeups/research do not start automatic compaction.
Custom non-SQLite stores retain blocking compaction.

A background provider error below threshold is visible but does not fail the
ordinary run. It is not retried within the same run; if compaction becomes
mandatory, that error stops the model request. Storage failure is stricter: the
next durable effect checkpoint fails even below threshold (including a tool
returned by an already in-flight ordinary model request). Empty/non-reducing summaries and
an indivisible final message/tool group that still exceeds the provider window
preserve the previous state.

Run snapshots expose preparing/ready/applied/stale/failed/cancelled. Each saved job
reports summary request count separately from ordinary model attempts. Provider
input usage is reported only when known for every request; interrupted or missing
usage remains unknown. No pricing estimates are invented.

## Verified crash behavior

The [durable recovery matrix](durable-recovery.md#crash-matrix) records real
SIGKILL tests at admission, routing, model/tool checkpoints, delivery, cancellation,
retry waits, wakeup reconciliation and compaction. Recovery is deliberately
conservative about external effects: it preserves a confirmed result, or reports
an unknown outcome instead of automatically repeating an unsafe tool. Later model
decisions can still request new actions; inspect external state before authorizing
a retry after an uncertain result. Full histories and saved image payloads are
rewritten at checkpoints; the measurement report documents that cost.
