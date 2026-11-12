# Troubleshooting

## Startup or configuration fails

For standalone/gateway mode, check `~/.kodrik/config.toml`, even if you started
Kodrik from a different directory. The file needs `[gateway]` (which may be
empty) and `[provider.primary]`. TOML strings containing `${VARIABLE}` are not expanded.
Validate companion timezone, hour values and paired recipient fields against
the [configuration reference](configuration.md).

## Remote TUI cannot connect or use the laptop

- Check `~/.kodrik/remote.toml` or the explicit `--remote-config`, not the local
  provider configuration. Use a trailing-slash HTTPS base URL (HTTP only on
  loopback), without credentials/query/fragment. For a proxy prefix, retain the
  prefix and forward API, SSE and WebSocket routes without redirects.
- Pair with `kodrik connect`; compare the request UUID/code in the owner's web
  profile within five minutes. Device credentials expire after 30 days. Revoke
  an old device in **Device connections**, explicitly remove its credential,
  then pair again. Never share the credential or retry an uncertain token
  response blindly. The credential must be a private regular file; its parent
  directory must also be private and not a symlink.
- Initial launch, `/new`, selection, edits and version switches leave local
  access detached. Check the session and project with `/attach`, then use
  `/attach yes` and approve any new manifest disclosure. Only the owner's same
  personal conversation can use it from web/Telegram, not guests or groups.
- After network loss or sleep, chat observation may reconnect but local access
  does not. Heartbeats are 5 seconds; the lease is 15 seconds. Explicitly attach
  again for a fresh generation, inspect uncertain effects, then send a new
  message. A paused run does not automatically resume or replay commands.
- Use `/retry` for unconfirmed sent/edited input, not a duplicate new message.
  This checks its original submission ID, not a tool's external outcome.
- `Cleanup worker exited` blocks local environment startup. Preserve the full
  error, including worker PID, status and bounded stdout/stderr diagnostics;
  do not delete receipts or automatically replay commands. For local development,
  use a separate `CARGO_TARGET_DIR` for each checkout so another build cannot
  replace an executable while its tests are running.
- A changed skill or project `AGENTS.md` requires detach/reattach. Required HTTP
  MCP failure blocks attachment; an explicitly optional MCP can be omitted with
  a warning. stdio MCP and remote `ask` are unsupported.
- Preserve `~/.kodrik/remote/` receipts when journal/storage errors occur; do not
  delete evidence to force a replay. Cleanup is best effort, not rollback. A
  disconnected local file cannot be fetched from its laptop path; only saved
  server attachments remain downloadable in Web Chat. Remote TUI cannot upload
  attachments itself.

See [Remote TUI](remote-tui.md) for configuration, data disclosure, backups and
the unverified real two-machine/sleep acceptance scenario.

## Provider rejects requests or images

The endpoint must implement the Responses API. A Chat Completions-only endpoint
is insufficient. Image requests additionally require `input_image` support,
including images returned from tools. Kodrik does not switch models or substitute
text after rejection. See [Images and vision](vision.md).

## A new topic stays in the same session

The foreground agent, not System One, must search for a suitable personal
session for an unrelated topic, then switch to it or create a new dialogue if
none fits. There is no idle threshold; legacy `auto_switch` and `idle_minutes`
settings have no effect. Check whether the message was accepted as steering in
an ongoing run or the conversation is outside the supported personal-session
scope. Handoffs also reject busy targets and stop after four switches per run.
Use `/new` for a definite fresh start. See [Sessions](sessions.md).

## Changed memory, personality or skills are not reflected

Sessions keep saved context snapshots. Start `/new` after changing these files.
The memory tool's `list` action reads live files, so its result can differ from
the old snapshot. Forgetting does not erase earlier conversation text.

For skills, check YAML metadata and folder naming. Invalid user skills are
skipped with a warning. A valid same-name user skill overrides a built-in.
See [Memory](memory.md) and [Skills](skills.md).

## Learning or initiative seems inactive

Learning skips short acknowledgements and turns while a review is already
running. Proposals need evidence, and full stores reject additions. Disabling
`learning_enabled` stops automatic reviews but leaves manual tools available.

Initiative requires a running gateway, a configured private recipient, an active
interest, a permitted local hour and ten minutes without gateway messages.
Only one attempt is allowed per local day. Cancellation or failure consumes
that attempt, and the model may choose silence. Inspect logs rather than
clearing `proactive.json` to force a retry. See [Companion](companion.md).

## “Update yourself” does not install new source

Check that the current session knows the `self-update` skill and that no user
override shadows it. The update stops on local changes, missing upstream,
detached HEAD, local commits ahead of upstream or fetch failure. A rebuild alone
does not fetch source, and a successful build still needs a process restart.
See [Updating and running Kodrik](operations.md).

## Find diagnostic logs

CLI and gateway initialize logging; the terminal UI has no log display:

```sh
RUST_LOG=warn kodrik ask "Hello"
RUST_LOG=info kodrik gateway
```

For a managed gateway, inspect the output destination configured by its service
manager. Kodrik does not define a universal log-file location. Before sharing
logs, remove credentials and private conversation content.

## Restart confirmation is missing

Cooperative restart state lives in `~/.kodrik/restart-request.json` before the
old gateway exits and `~/.kodrik/restart-receipt.json` until the new gateway
sends a new restart confirmation message. Inspect those files, the configured
gateway log, and `launchctl print gui/<uid>/com.kodrik.gateway`. Do not submit another restart
while either file is pending. Invalid receipts are renamed to
`restart-receipt.<id>.invalid.json` and do not stop the gateway.


## A durable run is parked or cannot be decoded

Use `kodrik runs --store local|owner|web list` with the actual store selector, then
`show <id>`. `crash_limit` means repeated interrupted attempts in one phase;
`automatic_recovery_disabled` means restart_limit is 0. Fix the underlying cause
before `retry <id>`, or use `cancel <id>` to stop it permanently. Retrying does not
reset a wakeup's original deadline and cannot make an unknown tool outcome safe.

`unsupported_run_version` or `invalid_run_checkpoint` blocks execution. Preserve
the database and use a compatible binary or restore a verified backup; do not
reset payload fields to defaults. Unknown newer database schemas also refuse to
open. Legacy version-1 run payloads lacking crash accounting start with an inactive
marker and zero attempts; no past crashes are invented. Current permission or
attachment failures remain blocked without consuming the crash budget.

## Storage failed or a tool outcome is unknown

A failed checkpoint stops subsequent effects. An error can occur even after a
transaction committed; the process must not assume rollback from the error alone.
Recovery rereads SQLite. An intent without a saved unsafe result becomes
interrupted/unknown; a saved result is reused. Check the external system before
asking the model to repeat the operation. Delivery `Uncertain` also prevents an
unconditional resend.

For READONLY/FULL failures, correct permissions or free space while retaining the
last confirmed database and sidecars. For corruption, stop writers and restore a
verified compatible backup; startup deliberately refuses to replace evidence with
an empty database. Unknown future schema requires the newer compatible binary or
a matching older backup, not an edited version number. Follow the
[incident procedure](operations.md#recovering-after-a-crash).

## Context overflow still fails

Only structured `context_length_exceeded` enables one automatic recovery attempt.
If compaction is disabled, the run is bounded, there is no eligible history, or
the summary cannot reduce context, reduce the input or start `/new`. A single
large file/tool group cannot always be summarized safely. Ordinary HTTP 400/413
and attachment errors are not interpreted as context overflow. See
[overflow recovery](sessions.md#one-recovery-attempt-after-context-overflow).
