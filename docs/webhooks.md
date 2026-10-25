# Webhooks

Webhooks let an external service submit arbitrary event data to the owner agent.
Enable the web interface, sign in as the owner, and open **Webhooks** in the chat
icon rail. The sidebar lists your webhooks; selecting one opens its instruction,
credentials and recent events in the main area. On mobile, select the icon to
open the list. Invited users cannot manage or invoke owner webhooks with their login.

Selecting a webhook gives it a dashboard link, `/app/webhooks/WEBHOOK_ID`.
Copy this address to reopen the same settings after a reload or in another tab.
Pages use `/app/pages/PAGE_SLUG`, and conversations use
`/app/chat/SESSION_ID` links. These dashboard links require your usual
login and contain no Bearer credentials; ingestion and published-page URLs are
separate.

## Create and manage

Choose **Create webhook**, enter its name and the instruction the agent should
follow, and save. The instruction can identify a project, repository, required
checks, and the expected result. For example:

> Investigate checkout errors in the shop repository. Reproduce the problem,
> add a regression test, fix the root cause, run the relevant checks and open a
> pull request. Include the incident URL and test results. If there is not enough
> evidence, report what is missing. Do not merge or deploy.

Copy the generated endpoint URL and Bearer token. The token disappears when you
select another webhook or leave the section. It is shown only in the creation
or rotation response and is not stored in browser storage. The server
stores its SHA-256 hash in the existing private SQLite session database.

You can edit the name/instruction, disable ingestion, rotate the token, or delete
the webhook without restarting Kodrik or editing configuration. Rotating a token
invalidates the previous one immediately. Disabling a webhook pauses ingestion;
reenabling it accepts its current token again. Deleting it permanently disables
its endpoint. Already accepted tasks and their conversations are preserved.

## Manage through the agent

When the web interface is enabled, the owner agent can use `webhooks` from
ordinary chats, Telegram, terminal mode and webhook runs. It shares the web
manager's database, validation and limits. Invited users, isolated agents,
groups and guest queries cannot use it.

| Action | Fields | Result |
| --- | --- | --- |
| `list` | None | `webhooks` array with settings and endpoint URLs |
| `get` | `id` | Webhook settings and endpoint URL |
| `create` | `name`, `instruction`, optional `enabled` (defaults to true) | Webhook settings, URL and new `token` |
| `update` | `id` and any of `name`, `instruction`, `enabled` | Updated settings; omitted/null fields stay unchanged |
| `delete` | `id` | `id` and `deleted: true` |
| `rotate_token` | `id` | New `token`; the old token stops working immediately |
| `events` | `id` | `events` array with the last 30 accepted deliveries |

Pass null for unused fields. For example, disable an existing webhook without
changing its instruction:

```json
{"action":"update","id":"WEBHOOK_ID","name":null,"instruction":null,"enabled":false}
```

Names must contain 1–200 bytes and instructions 1–16384 bytes; neither may be
blank. `list` and `get` never return credentials. Creating or rotating a token
returns its plaintext to the agent, so it is saved in conversation/tool history
and sent to the configured model provider. Share it only with the owner or the
intended sender. The webhook record still stores only its SHA-256 hash.

Interrupted tool calls are not automatically replayed. After an uncertain change,
inspect `list`/`get` before trying again. A token lost during creation or rotation
cannot be recovered from the webhook record; rotate it explicitly if needed.
Terminal mode can manage settings but does not start the HTTP server: the gateway
must be running to receive deliveries.

## Send an event

Send a POST request to the URL shown in the interface. Include the token in the
Authorization header, never in the URL. Remote access requires HTTPS; HTTP is
supported only for local development on loopback, as with the existing web UI.

```sh
curl -X POST 'https://kodrik.example/api/webhooks/WEBHOOK_ID' \
  -H 'Authorization: Bearer YOUR_TOKEN' \
  -H 'Content-Type: text/plain' \
  -H 'Idempotency-Key: incident-42-event-1' \
  --data-binary @event.txt
```

The body must be nonempty UTF-8 text of at most 64 KiB. It can contain plain text,
JSON with any structure, logs, stack traces, or other textual event data. No JSON
schema or Content-Type is required. The original text is passed to the agent
without parsing or rewriting it. Binary attachments are not supported.

The agent also receives the webhook's name, ID and receipt time separately from
the body. These details are snapshotted when the event is accepted, so renaming
or deleting the webhook does not change the recorded source. Subsequent messages
in its conversation are the owner's follow-up questions, not new webhook events.

## Send a notification to another channel

Owner agents can use the `messages` tool from webhook runs and ordinary chats.
Put the delivery request in the webhook's saved instruction, for example:

> Investigate the incident. Send me a short summary and the PR link in Telegram
> using messages. If the fix cannot be completed, send the reason instead.

`list_channels` discovers the configured destinations. `send` takes a channel
and nonempty text (up to 32 KiB):

```json
{"action":"send","channel":"telegram","text":"Checkout fixed: PR link","conversation_id":null}
```

Telegram sends through the bot selected by the existing owner settings to the
owner's private chat. No recipient ID or bot token is supplied by the model.
Telegram is absent when no owner/bot is configured. The owner must have started
the bot and must not have blocked it.

After a webhook notification is successfully delivered to Telegram, that private
chat selects the webhook's source session. The notification's tool call and
subsequent Telegram replies share the original event history and saved webhook
instruction. Receiving a webhook alone, sending to web, a pending delivery or a
failed send does not switch Telegram. Retrying an already delivered notification
does not override a session selected later by the owner.
If Telegram is still finishing a turn in its previous session, the new reply
waits for that turn and remains bound to the webhook session. While the webhook
run itself is still active, the existing cross-channel session-busy check applies.

The `web` channel is available when the web interface is enabled. Supply an
existing conversation UUID, or null to create a new conversation. A successful
send returns its conversation ID and dashboard URL. Messages appear as assistant
messages and remain in history. A busy destination waits until its current work
finishes; this does not invoke another agent turn in the destination.

`sent` confirms delivery, while `pending` means the message is saved for later
delivery. Pending notifications are checked every five seconds while Kodrik
runs and survive restarts. Rate-limited sends wait for the server's Retry-After.
Pending sends to a removed channel or a previously configured owner are rejected;
they are never redirected to the new owner. Deleting a destination web conversation
also rejects its pending messages.
An uncertain remote outcome is reported as an error and is not automatically
resent. A replay of the same tool call reuses its original receipt. Sending to
another channel does not suppress the final answer in the source conversation.

The tool is unavailable to invited users, groups and guest queries. This version
supports text; additional external channels can use the existing `Interface`
transport without changing the tool arguments. The separate `telegram_reply`
tool continues to reply only within its current Telegram chat.

A successfully persisted request returns `202 Accepted`:

```json
{"conversation_id":"SESSION_UUID","duplicate":false}
```

This acknowledges durable acceptance, not successful completion of the task.
Open **Recent events** in the webhook manager to see queue/execution status and
open the corresponding conversation. The normal chat history, live updates and
stop controls work for these conversations.

## Retries and recovery

An optional `Idempotency-Key` header accepts 1–128 ASCII characters. Reuse it when
retrying the same delivery. For that webhook, an identical body returns the same
conversation with `duplicate:true`; a different body returns `409 Conflict`.
Without the header, every POST starts a new conversation.

An error group can contain several distinct events. Choose delivery keys that
identify individual events; automatic grouping into incidents or existing PRs
is not part of this endpoint.

The session, owner instruction snapshot, queue submission and deduplication
receipt are committed in one SQLite transaction before returning 202. Accepted
work continues if the sender disconnects and is recovered after a process
restart using the existing agent recovery rules. Changing the webhook's
instruction affects future deliveries only. If the owner deletes a conversation,
its receipt still prevents duplicate execution; a retry then returns a null
conversation ID.

## Limits and permissions

- Up to 32 webhooks.
- Up to 10 new deliveries per webhook per minute; identical retries do not consume
  this allowance.
- Up to 100 outstanding web conversations per server process, sharing the
  existing four-worker execution capacity. Run one web server for this limit.
- Webhook turns have no webhook-specific model-call budget or execution deadline.
  Unfinished runs recovered after a restart also discard these legacy limits.

Missing, invalid, disabled or revoked credentials return 401. Invalid body/key
returns 400, an oversized body 413, a conflicting retry 409, and an exhausted
rate/queue allowance 429. Management requires the owner session cookie, and
management mutations require the existing same-origin checks. A browser login
cookie alone cannot authorize ingestion.

Webhook runs use the owner's existing tools, process permissions, context and
credentials. A webhook does not create a separate sandbox or repository access
policy. The stored instruction is separate from the untrusted event body, but
instruction text is not an operating-system permission boundary. Configure the
Kodrik process and credentials accordingly. Repository access and Git hosting
credentials must already be available if the task needs to create a PR.

Keep tokens private: anyone holding a webhook token can submit tasks to that
webhook. Do not include secrets in event bodies unless they are intended to be
saved in conversation history and sent to the configured model provider.
