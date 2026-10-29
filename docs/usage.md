# Using Kodrik

## Launch modes

These commands assume the executable is on `PATH`. From a source checkout,
substitute `./target/release/kodrik`.

| Command | Behavior |
| --- | --- |
| `kodrik` | Open the terminal UI |
| `kodrik ask "prompt"` | Run a one-shot conversation and save it on success |
| `kodrik gateway` | Run Telegram gateways, daily initiative and the enabled web server |
| `kodrik web set-password` | Set the owner web password interactively and revoke web sessions |
| `kodrik --help` | Show CLI help |
| `kodrik --version` | Show the executable version |

Launch from the directory where you want shell and file tools to work. State
still lives in `~/.kodrik/`; see [Storage](storage.md).

## Browser chat

Enable the web server and open `/app/chat` at your configured base URL. Sign in
with **Войти через Telegram** or the owner password, create or select a
conversation, and send text with
Enter (Shift+Enter for a newline). Responses stream, tool rows expand to show
details, and Stop cancels the active run while preserving saved history.
Personal history is shared with Telegram; CLI/TUI always uses the configured owner.
Replies stay in the interface that accepted the request.
See [Web chat](web-chat.md) for setup, login and limits.
Use the language selector for English or Russian; the browser remembers your
choice. See [Interface languages](languages.md) for detection and saved preferences.

## Terminal UI

An interrupted request resumes with history already visible and a “Recovering
session” status. Scrolling, selection and draft editing remain available while
recovery waits for the model. Enter leaves the draft unsent until recovery finishes;
Esc stops recovery, and Ctrl+C with empty input exits while keeping it resumable.

| Input | Action |
| --- | --- |
| Enter | Send the message |
| Shift+Enter, Alt+Enter or Ctrl+J | Insert a newline |
| `/new` | Start a fresh session |
| `/sessions` | Open the session picker |
| `/language en` or `/language ru` | Change and save the interface language locally |
| ↑ / ↓ in the picker | Select a session |
| Enter / Esc in the picker | Open / close |
| Esc while working, with no command menu open | Cancel the current request |
| Ctrl+C | Clear nonempty input; quit when input is empty |
| Left-button drag anywhere in the interface | Select text; releasing the button copies it and clears the selection on success |
| Esc with a selection | Clear the selection before stopping a request or closing a menu |
| Ctrl+C with a selection | Copy the selection again |
| Mouse wheel | Scroll the conversation; move the selection in the session picker |
| ↑ / ↓ with empty input and no menu open | Scroll the conversation |
| ↑ / ↓ with nonempty input | Move the input cursor between lines |
| PageUp / PageDown | Scroll the conversation by ten lines |

The wheel scrolls history independently of the input cursor. Scrolling stops at
the beginning and end. Reading earlier messages holds the current position while
new output arrives; at the bottom, the view follows new output.

Text is selectable throughout the interface, including tool rows, input, menus
and status lines. Drag to a history edge or use the wheel while dragging to select
offscreen text. Copying preserves explicit newlines without inserting newlines at
soft wraps. Input selection copies text without editing it. A click alone does
not change the clipboard.

While a selection is active, displayed text is held steady; the agent continues
working. A successful native copy or terminal copy request clears that selection
and reveals accumulated output. Copy failures retain it for retry with Ctrl+C;
completion of an older copy leaves a newer selection intact. Esc or a new keyboard
action also clears selection, as do resize and focus loss. Keyboard paste remains
available.

Copying uses native clipboard tools locally (`pbcopy` on macOS, PowerShell on
Windows/WSL, `wl-copy`, `xclip` or `xsel` on Linux). Remote SSH, tmux and Herdr
sessions use OSC 52 to request a copy from the attached terminal; this requires
terminal/multiplexer clipboard support and permission. The terminal fallback
accepts up to 100,000 UTF-8 bytes. A “Copy requested from terminal” notice confirms
the request was sent, not that the terminal accepted it. Native copy completion
shows “Copied to clipboard”. Copy notices appear in the bottom-right corner; the model name and context token
count remain on the left.

Tab or Enter completes a selected slash-command suggestion; Enter submits the
completed command.

You can send corrections while the agent is working; they join the current run.
`/new` and `/sessions` are unavailable during that run. After a sufficient pause,
[automatic routing](sessions.md) may start a new session for an unrelated topic.

## Telegram

Configure the bot in [Configuration](configuration.md), then run
`kodrik gateway`. Send text or a photo with an optional caption.

| Command | Action |
| --- | --- |
| `/new` | Cancel current work/downloads and start a fresh session |
| `/stop` | Cancel current work/downloads |
| `/chatid` | Show the current chat or group ID; no model call |
| `/sessions [page]` | Open the personal dialog picker with buttons in a private chat |
| `/session <id>` | Select a personal session without replaying its history |
| `/language en` or `/language ru` | Save this chat's interface language; no model call |

For web login, choose **Войти через Telegram** in the
browser. Open the bot with the button or QR code, compare the site and request
code, then confirm with **Войти**. The original tab signs in automatically.
Requests expire in five minutes; **Отказать** refuses the request.

In a group, use `/chatid@YourBotUsername` to address your bot explicitly.

For each ordinary group message, Kodrik first decides whether to participate,
using sender identity, mentions, replies and up to 16,000 characters of recent
conversation text. It normally
answers direct requests and continuations addressed to it, stays silent when
people address someone else, and joins questions to the group only when a reply
is clearly useful. Ambiguous decisions, provider errors and decision timeouts
stay silent. This is a model judgment and can misidentify the addressee.

Silently observed messages and attachments remain in chat history. The decision
does not run tools or show typing/progress; attachments are downloaded as usual,
but their contents are not sent to the decision model. Group messages received
during work wait for separate decisions after that turn instead of steering it.
Stopping work saves queued group messages silently; `/new` saves them in the
previous session before switching. Private chats and `/chatid` retain their
existing behavior.

Group decisions use `provider.system_one` when configured, otherwise `primary`,
and incur model usage even when Kodrik stays silent. They remain enabled when
automatic session switching is disabled. To let the bot see ordinary group
messages, disable its Privacy Mode in BotFather or make it an administrator;
see the [Telegram bot FAQ](https://core.telegram.org/bots/faq#what-messages-will-my-bot-get).

Use `/sessions` and tap a dialog to continue a web or terminal conversation
in a private Telegram chat. The picker shows the current dialog, dates and busy
indicators, with web pins first and recent conversations next. Web message versions
appear as one dialog. Navigation edits the same menu; successful selection or
creation replaces it with a short confirmation and removes the buttons. Errors
keep the picker available. Selection never sends old messages.
The new-dialog button uses the same `/new`
control, including interrupting current work; duplicate callbacks do not create
extra dialogs. `/session <id>` remains available for manual selection.
Each interface retains its own selection. Ask the agent to search earlier
conversations when you need excerpts. See [Images and vision](vision.md) for photo limits.
After the first successful private Telegram answer, a short title is generated in
the background using the same rules and storage as web chat. Existing untitled
sessions get a title after their next answer; manual names are preserved.

The plan and current tool command appear in one edited progress message, with a
blank line before the command. Updates are coalesced to roughly one per second.
Command arguments are previewed up to 160 characters; full tool results are not
shown there. A plan remains with its completion status, and the answer arrives
as a separate message.

Ask Kodrik to send a photo, video, file, audio, poll, location or formatted post.
Ask it to send several photos together to receive one album of 2–10 photos in
the requested order, with an optional shared caption on the first photo.
The `telegram_reply` tool sends directly to the current
chat; the agent cannot select another recipient. A successful explicit send is
the reply, so its final text is not sent again.

Media can come from an existing file, an HTTP(S) URL or a Telegram file ID.
Owner chats can use local files; isolated chats can use only files in their own
virtual filesystem. Uploads are limited to 10 MB for photos and 50 MB for other
media, with captions up to 1024 characters. This tool sends existing media;
it does not generate images, audio or video.

For an album, `telegram_reply` accepts `message.type: "photo_album"`, an ordered
`media` array of the same file/URL/Telegram sources, and `caption` (or `null`).
Every photo is validated and every local file is loaded before sending the
album in one request. The returned `message_id` identifies the first photo.

Posts use rich Markdown inside the chat. Polls use `sendPoll`.

Ask “What are the poll results?”. The `telegram_results` tool lists the 20 most
recently updated polls in the current chat, then reads details by ID. Results
include the latest received counts and their update time. Votes save state
without calling the model; the agent reads results when asked. Data survives
`/new` and gateway restarts.

Polls are anonymous by default. Ask for a non-anonymous poll to make individual
choices available; the agent can set `anonymous:false` when creating it. Vote
changes and withdrawals replace previous choices. These are saved snapshots,
not live fetches: individual votes can be incomplete if the bot missed updates.
Only polls created and registered by this version of the bot are available.
`counts_updated_at` dates the aggregate counts; `updated_at` dates the latest
saved activity, which may be a vote without newer aggregate counts. Individual
votes are read in pages of 50 using `offset` and `next_offset`; all saved votes
remain available across pages. Use `offset:null` for the first page and lists.

Guest mode in private chats is available to the owner and invited users. The two
participants share one guest history, memory, datasets and virtual filesystem,
without access to personal memory or files. The agent distinguishes speakers by
Telegram user ID, with their name and username supplied as descriptive labels.
For example, either person can ask to add a film to their shared watchlist, or
create and update a shared shopping checklist. Other pairs have separate spaces.
Ordinary groups remain disabled.

Guest mode only supplies invocations and quoted reply context, not the full private
chat history. Mention the bot in the request, or reply to the relevant film or
shopping message while invoking it.

Guest mode has one response slot. It supports posts, locations and media from
public URLs or Telegram file IDs, but not local uploads, photo albums or polls.
Guest document URLs must identify PDF or ZIP files, and guest video URLs require
a JPEG thumbnail URL. If delivery is uncertain, Kodrik does not retry that send
or use a second guest response.

## Available tools

The agent receives the current date, time and weekday automatically before each
model call, using `companion.timezone`. This also applies to resumed sessions and
steps after tool execution. The clock context is not saved in session snapshots.

The agent chooses tools from your request; these names are not slash commands.

| Tool | Purpose |
| --- | --- |
| `update_plan` | Show and update a task plan |
| `spawn_agent` | Start an independent child session and return its ID immediately |
| `send_message` | Steer an active child or start a follow-up in its existing session |
| `list_agents` | Inspect this session's children, statuses and results |
| `wait_agent` | Wait up to 60 seconds for a child result |
| `stop_agent` | Cancel a child and its queued inputs while retaining its history |
| `telegram_reply` | Send media, polls, locations or posts to the active Telegram chat, or to the owner's private Telegram chat from other interfaces |
| `telegram_results` | Read saved poll results in the active Telegram chat or the configured owner's private chat; unavailable in guest mode |
| `exec_command` | Run commands in Bashkit for isolated chats; native shell for CLI/TUI and the owner's private chat |
| `apply_patch` | Edit files |
| `publish_page` | Publish or update a public/private web page; available to the owner across interfaces when web is enabled |
| `ui_components` | Discover available Nuxt UI tags and inspect a component's package API and MDC adapter rules |
| `show_ui` | Render native MDC components in chat; optionally publish a page with private dataset forms |
| `view_image` | Inspect a local image |
| `memory` | Save, inspect, replace or remove explicit memory |
| `datasets` | Keep typed diary/tracker records, query bounded pages and calculate summaries within the current space |
| `companion` | Inspect learning, forget lessons and manage interests |
| `wakeups` | Schedule, list, reschedule or cancel the agent's own one-shot follow-up work in the source Telegram, web, CLI/TUI or webhook conversation |
| `session_search` | Search or browse previous user/assistant messages |
| `mcp_search` | Find configured external MCP tools by name/description and load matching schemas for the next model request |
| `encyclopedia_search` | Research topics on English or Russian Wikipedia |
| `web_search` | Search the configured SearXNG instance for public sources; requires research configuration |
| `web_read` | Read a public page as Markdown using local Obscura; requires research configuration |

For example: “Find our earlier discussion about cities”, “Show your learned
facts and their sources”, or “Use skill-creator to create a release-notes skill”.
See [Memory](memory.md), [Datasets](datasets.md), [Companion](companion.md), [Skills](skills.md) and
[Web research](web-research.md).

## Child agent sessions

The web chat shows child tasks above the message composer. Expand a task to read
its instruction, result or error; statuses update live, including while the
parent is waiting. Stop cancels only the selected child, leaving the parent
conversation and your draft intact. Cancellation stays visible until pending
effects finish. Completed actions are not undone. Send corrections and follow-up
instructions through the main chat as usual. Technical tool details remain
available in the message's expandable steps.

Ask Kodrik to split independent work between subagents, for example: “Have one
agent inspect authentication and another inspect its tests; collect both results.”
The parent can continue working while children run. Each child has its own
history and inherits the parent's instruction, memory and skill snapshots and
the same execution sandbox. The conversation history is not copied: the parent
passes the needed context in the task. Children can edit files, including shared
host files in owner/CLI mode, so assign non-overlapping changes.

`spawn_agent` takes `task` and returns `id` and `status`. `send_message` takes
`agent_id` and `message`; corrections are persisted before acknowledgement and
consumed between execution steps. A message to a finished child starts another
turn in that history. `list_agents` takes no arguments. `wait_agent` takes
`agent_id` and nullable `timeout_ms` (default 10000, range 1–60000); a timeout
returns `running` without stopping work. `stop_agent` takes `agent_id` and waits
up to 60 seconds for cancellation; if still running, inspect or wait again.
Other statuses are `completed`, `stopped`, `failed` and `interrupted`.

Tasks/messages must contain 1–16384 UTF-8 bytes. Up to three children can run
concurrently per application worker, with sixteen child sessions per parent
session. Each child turn has sixteen model attempts, five minutes and a bounded
input context. Returned results/errors are limited to 16384 characters; the full
history remains in SQLite. Children cannot delegate, reply directly to the user,
schedule wakeups, manage webhooks or run automatic learning reviews.

Active children stop when the parent's current run completes, fails or is
cancelled. The parent should collect their results before finishing. Completed
histories can be reused in later turns of the same parent session. Child access
is bound to the parent's session ID and generation; resetting the parent does
not grant access to its previous children. Internal `companion:subagent:*`
histories do not appear in ordinary chat lists or session searches.
Deleting a parent chat also deletes its child histories; active children must
finish or be stopped before deletion.

Child transcripts, accepted messages and execution checkpoints survive process
restarts. Interrupted work is not automatically replayed: the next inspection
marks abandoned execution `interrupted` and cancels its pending inputs. Inspect
external state before sending a new instruction, because a tool may have taken
effect before interruption. An uncertain spawn/send is not safe to repeat
blindly; inspect `list_agents` first. Stopping a child does not undo its changes.

## Agent wakeups

Ask “Check the build in twenty minutes”, “Show your scheduled follow-ups”,
or “Cancel that build check”. Kodrik may also schedule a concrete follow-up on its
own initiative. It confirms scheduling only after saving. The default timezone
is `companion.timezone`.

Wakeups execute with the originating chat's tools and skills in a separate internal
session. They may finish silently or send a useful message to the originating
chat. Web, CLI/TUI and webhook results are saved in the original conversation
history. Tasks survive `/new` and restarts; execution requires a running Kodrik
process for the same user. Keep the gateway running for unattended work. Only
pending tasks can be rescheduled or cancelled. Recurring schedules are unsupported.
Use the installed `remindctl` skill for user-facing Apple Reminders. See
[execution and delivery](companion.md#agent-wakeups).
