# Updating and running Kodrik

## Homebrew installations

For the Apple Silicon package, use [Homebrew update and restart](homebrew.md#update-and-restart).
It updates through `brew upgrade`, runs an optional user service through
`brew services`, and preserves `~/.kodrik/`. The bundled `self-update` skill
identifies the running installation before choosing Homebrew or the source
workflow below. Do not run the source installer over a brew-managed service.

## Update, build and restart are separate operations

The bundled `self-update` skill distinguishes three requests:

| Request | Intended operation |
| --- | --- |
| “Update yourself” | Update with Homebrew, or fetch/fast-forward/build the configured source checkout, then handle a verified restart |
| “Build the current revision” | Build the checked-out source; report local modifications |
| “Restart” | Restart the existing executable without fetching or rebuilding |

Explicit constraints in your request take precedence. The skill is guidance used
by the agent through its shell tools, not a dedicated CLI subcommand.

## Update prerequisites

Automatic source updates require a known repository, an attached branch with an
upstream, and a clean worktree/index with no untracked files. Local commits ahead
of upstream stop the update. The skill does not stash changes, reset the branch
or resolve divergence automatically. A failed fetch must not be reported as an
up-to-date installation.

To inspect and update a clean checkout manually:

```sh
git status --short
git branch --show-current
git rev-parse --abbrev-ref --symbolic-full-name '@{upstream}'
git fetch
git rev-list --left-right --count 'HEAD...@{upstream}'
```

Proceed only if the status is clean, the branch/upstream are known and the first
count (local commits ahead) is zero:

```sh
git pull --ff-only
npm ci --prefix web
npm run build --prefix web
cargo build --release --locked
./target/release/kodrik --version
```

Restart the process that actually serves your conversations using its verified
executable path. Building a new file does not replace an already running process.

## Gateway lifecycle

### Telegram attachments

Send a document with an optional caption. PDF, Word/ODT/RTF, Pages, PowerPoint/
Keynote, Excel/CSV/TSV/IIF, and supported text/code files go directly to the
configured Responses API as `input_file`. This requires an endpoint supporting
[OpenAI file inputs](https://developers.openai.com/api/docs/guides/file-inputs),
and a vision-capable model for PDF page images. Non-PDF documents supply text,
not embedded pictures/charts. PNG/JPEG/WebP documents use image input instead.
Unsupported formats and failed downloads produce a reply rather than being ignored.

Documents are limited to 20 MB by Telegram's hosted Bot API. Images retain the
10 MiB / 32-million-pixel limit. File inputs across the current conversation
must total no more than 50 MB; use `/new` if the conversation exceeds this.
Original document bytes are saved privately in `~/.kodrik/attachments/` with
generated storage names. Saved sessions reference those files; keep them when
backing up or moving state. They are not automatically deleted, because old
sessions may still need them. No Python or local document converter is used.

### Service installation

For the brew package, follow [Homebrew service setup](homebrew.md#run-the-background-gateway).
Both installation methods use `com.kodrik.gateway`; migrate explicitly instead
of starting a second service. The instructions below concern the source installer.

On macOS, run `./scripts/install-macos.sh` from outside the running gateway.
It builds and installs the current checkout in `~/.local/bin/kodrik`, writes
`~/Library/LaunchAgents/com.kodrik.gateway.plist`, and starts the service when
configuration already exists. The checkout remains its working directory.
Existing repository `data/` logs are moved into `~/.kodrik/logs/`; an empty
`data/` directory is removed. The previous plist is backed up to
`~/.kodrik/gateway.plist.backup`. Rust and a supported Node.js version
(22.19+ on 22.x, 24.11+ on 24.x, or 26+) must be installed;
no Python is required. The installer builds the web frontend before Rust.

For a foreground gateway:

```sh
RUST_LOG=info ./target/release/kodrik gateway
```

For background operation, use the service manager appropriate to your machine.
The macOS installer creates the service; the self-update skill includes checks
for a macOS launchd service named `com.kodrik.gateway`; it must verify the actual
service and executable before restarting. Other launch mechanisms need to be
identified first.

A gateway cannot verify its own replacement before it exits. A scheduled
self-restart is not proof of success. When an update runs inside the gateway,
the new binary records a PID-targeted request. After the final response is saved
and delivered, the gateway drains admitted work and exits; launchd `KeepAlive`
starts the new binary. The new process sends a separate confirmation with its PID.
Graceful shutdown lets a running learning review finish; forceful termination
can lose that review.

The web server allows HTTP connections five seconds to drain after shutdown is
requested, so incomplete requests and stalled clients cannot indefinitely delay
the process exit. Requests still in flight may lose their connection; admitted
chat messages remain in durable storage. This limit does not cancel checkpointing
or the learning review. With `RUST_LOG=info`, gateway logs show the restart signal,
worker draining and learning phases; an HTTP drain timeout is logged as a warning.

The first binary with cooperative restart support needs one controlled manual
restart and `/new` to load the updated bundled skill into the conversation.
Later in-gateway updates use `kodrik request-restart <pid>` and must not create
helper launchd jobs, detached processes or fixed-delay signals. Cooperative
restart requires the verified launchd job to use `KeepAlive`.

## Refresh built-in skills

Skills in the source `skills/` directory are embedded at build time. Rebuild and
restart to use a new bundle, then start `/new` to refresh the session's catalog.
A valid user skill of the same name overrides the built-in. Check for an override
if an updated skill appears ineffective.

See [Skills](skills.md) for cache repair, [Storage](storage.md) for backups and
[Troubleshooting](troubleshooting.md) for logs.


## Inspecting and controlling durable runs

Choose an explicit store; run IDs never search other spaces:

```sh
kodrik runs --store local list
kodrik runs --store owner show <id>
kodrik runs --store owner retry <id>
kodrik runs --store web cancel <id>
```

`local` selects local sessions only when no owner is configured. `owner` selects
only the configured owner's migrated store; an incomplete migration must be
resolved by normal application startup. `web` is a compatibility alias for the
same owner's store, or local sessions when no owner is configured. These commands neither initialize providers nor execute tools. They
do not expose arbitrary store paths or access another Telegram user's space.
Missing databases and symlinked space directories are rejected.

List/show output JSON metadata: run/session/source IDs, phase, a fixed reason
category, consecutive recovery attempts, model attempts, deadline, cancellation
flag and the last confirmed tool call ID (null when none). Message bodies, tool
arguments/results, provider tokens and raw failure text are omitted.

A run parks after `[sessions].restart_limit` consecutive crashed attempts in the
same semantic phase (default 5). A persisted active marker plus released OS locks
identifies an interrupted attempt. Successful movement to the next semantic phase
resets the count; writing the same tool intent, streaming deltas, audit and
heartbeats do not. Clean shutdown clears the marker. Busy workers, permission
blocks and opening/closing the store do not consume attempts. Provider retries
use their separate existing budget.

`retry` accepts only Parked, non-cancelled runs and restores their interrupted
phase with one manual allowance, including when restart_limit is 0. It preserves
wakeup deadlines/model budgets, isolation provenance and tool replay policies.
Normal recovery must then run: restart the relevant CLI/TUI/gateway/web service,
or trigger the matching conversation's recovery. The wakeup scheduler checks on
its next tick. The command itself never executes a recovered run with local CLI
privileges. Parked runs continue to hold their conversation's durable admission.

`cancel` records an irreversible cancellation. Idle runs are finalized under the
same ownership locks as runtime; active workers observe the cancellation marker.
It cannot undo an external action already in flight or revive a terminal run.

Recovery/park/retry events are private JSONL records under the selected store's
`recovery-audit/`, containing stable IDs and fixed categories. Reopening an already
parked run produces no new park event. The checkpoint commits before its audit
append; a crash in that gap can omit the event, so SQLite is authoritative.

## Recovering after a crash

1. Identify the affected space and use `kodrik runs --store local|owner|web list`,
   then `show <id>`. Choose one real selector rather than passing the literal
   alternatives. SQLite is authoritative; a missing JSONL audit finish is not
   proof that the effect failed.
2. For an interrupted unsafe tool or an Uncertain delivery, inspect the actual
   destination first: files/commits, remote job ID, published page or recipient
   message history. Record what happened before requesting any new action.
   `retry` cannot turn an unknown effect into a safe replay.
3. Correct storage permissions/free space, missing attachments or the underlying
   crash cause. Keep the failed database and take a private backup before repair.
4. A parked run can be explicitly retried with the matching store's `retry <id>`;
   startup of the matching client performs recovery. Cancel the run if its work
   is no longer wanted. Cancellation does not undo already accepted effects.
5. Reconnect the client and inspect its canonical status. Do not submit the same
   operation with a new request ID just because the original HTTP reply was lost.

To stop an active conversation, use its Stop action or `/stop`; this persists
cancellation and cancels its owned pending summary. Closing the browser is detach.
Cancel future wakeups with the wakeups tool; cancelling today's run does not
remove independent future tasks. To stop *all* background work, stop every
Kodrik process through its launcher/service manager and keep automatic restarts
disabled during maintenance. Turning off `companion.proactive_enabled` or
`companion.learning_enabled` prevents their future starts after configuration
reload; it is not a substitute for stopping a currently running process.

A binary rollback requires a compatible coordinated backup. Never delete SQLite
and restart an old JSON-only binary. Reconcile effects after the backup timestamp
before restoring execution. See [Storage](storage.md#back-up-and-restore) and the
[crash matrix](durable-recovery.md#crash-matrix) for the tested guarantees and limits.
