<picture>
  <source media="(prefers-color-scheme: dark)" srcset="web/public/chat/logo-dark.svg">
  <img src="web/public/chat/logo.svg" alt="kodrik logo" width="312" height="84">
</picture>
<hr>

Kodrik is a personal AI companion built in Rust. It supports broad conversations,
remembers preferences, learns reusable procedures and explores interests.
Programming is one of its abilities. Use it in a terminal, through Telegram or in
the browser, with personal history, memory and skills shared across interfaces.

- Persistent memory, personality and searchable conversation history.
- Skills for reusable workflows, including skill creation and self-update.
- Saved conversations, automatic context compaction and topic-based session switching
  after a pause, with an optional fast decision model.
- Image understanding from local files, Telegram photos and browser uploads;
  PDF, Office and text/code attachments in Telegram and web chat.
- Structured datasets for diaries and trackers, with queries and summaries.
- Tables, charts and interactive UI in browser chat; persistent public/private
  pages and dashboards, including private forms that save dataset records.
- Browser chat with streaming responses, tool details, cancellation, search,
  pinned dialogs and message editing that preserves previous conversation branches.
- Shared personal dialogs in Telegram and the browser, with a Telegram session picker
  and web sign-in through Telegram or the owner password.
- Optional daily Telegram initiative based on saved interests and one-shot reminders.
- Optional web search through self-hosted SearXNG and page reading with local Obscura.

Images and documents are sent to the configured model provider for analysis.
Images require a vision-capable model; documents require file-input support.
PDF page images also require vision support.

## Quick start

Source repository: [suinly/kodrik](https://github.com/suinly/kodrik).

You need credentials for a model and endpoint supporting the OpenAI Responses API
and tool calling. An endpoint that only supports Chat Completions is insufficient.

### Install with Homebrew (Apple Silicon)

The Homebrew package currently targets macOS 14 Sonoma or newer on Apple Silicon.
This is the package's chosen support floor; older macOS versions have not been
validated. Intel Macs and Linux are not included in this release pipeline.

```sh
brew install suinly/tap/kodrik
kodrik --version
```

The executable includes the web frontend and built-in skills. Rust, Node.js and a
source checkout are not required. Installation does not create your configuration
or start a background service. Continue with **Configure and run** below.

### Build from source

Source builds require a current stable Rust toolchain with Rust 2024 edition
support, npm, and Node.js 22.22.3+ on 22.x, 24.15+ on 24.x, or 26+.

From the repository root, build the frontend before Rust; web assets are embedded
in the executable, so no Node.js server is needed at runtime:

```sh
npm ci --prefix web
npm run build --prefix web
cargo build --release --locked
export PATH="$PWD/target/release:$PATH"
```

The final command makes the built `kodrik` executable available in this shell.
You can also invoke `./target/release/kodrik` directly from the checkout.

### Configure and run

For either installation method, create the private state directory:

```sh
install -d -m 700 ~/.kodrik
```

Create `~/.kodrik/config.toml`:

```toml
[gateway]

[provider.primary]
type = "openai"
model = "your-model"
api_key = "your-key"
```

Replace the model and API key placeholders and run
`chmod 600 ~/.kodrik/config.toml`. The empty `[gateway]` table is
required even for terminal-only use. For a custom endpoint, set `api_base` under
`[provider.primary]`; see [Configuration](docs/configuration.md).

Launch the terminal UI or ask a single question:

```sh
kodrik
# Or ask a single question:
kodrik ask "What should we explore today?"
```

Use `/new` for a fresh conversation and `/sessions` to reopen a saved one.
Launch from the directory where you want shell and file tools to work.

The web, TUI and Telegram interfaces support English and Russian. See
[Interface languages](docs/languages.md) for detection and manual selection.

### Telegram and browser chat

To enable Telegram, add a bot to `~/.kodrik/config.toml`:

```toml
[gateway.telegram]
type = "telegram"
token = "your-bot-token"
```

To enable browser chat and published pages locally, add:

```toml
[web]
enabled = true
listen = "127.0.0.1:8080"
base_url = "http://127.0.0.1:8080/"
```

Set the owner password if using password login, then run the gateway:

```sh
kodrik web set-password
kodrik gateway
```

Open [the local chat](http://127.0.0.1:8080/app/chat) and sign in with the owner
password or through your configured Telegram bot. The gateway serves both
Telegram and the enabled web interface; terminal mode does not start the server.
For remote access, configure HTTPS following [Web pages](docs/web-pages.md).

The owner can manage Bearer-authenticated webhooks from **Webhooks** in the chat
sidebar or ask the agent to manage them using its `webhooks` tool.
Each delivery starts a separate agent conversation using your saved
instruction. See [Webhooks](docs/webhooks.md) for request examples and limits.
The owner agent can use `messages` to send text to the owner's Telegram chat or
an existing/new web conversation, including when started by a webhook.

For a Homebrew installation, run the gateway in the background after configuration:

```sh
install -d -m 700 ~/.kodrik/workspace ~/.kodrik/logs
brew services start suinly/tap/kodrik
brew services info suinly/tap/kodrik
```

Stop the foreground gateway first. Run the service commands as your regular user,
without `sudo`. The background gateway works in `~/.kodrik/workspace` and writes
logs to `~/.kodrik/logs/`. If you previously used the source installer, follow
[migration instructions](docs/homebrew.md#migrate-from-the-source-installer)
before starting the brew service.

Configuration and state live in `~/.kodrik/`. Each user shares personal history,
memory and skills between Telegram and web; CLI/TUI always uses the configured
owner. Access requires an owner-issued invitation; groups are disabled. The owner
and invited users can invoke guest mode in private chats with other people. Each
pair shares guest history, memory, lists and sandboxed tools, separate from their
personal memory and files. The agent identifies each speaker by Telegram user ID.
Invited users use Bashkit with a persistent
virtual filesystem; CLI/TUI and the configured owner's personal chats retain
native shell and file tools. Set `[owner]` to link your Telegram identity to the
owner's state and native tools; without it, all Telegram chats use Bashkit.
Create a separate link for each person with `kodrik invites create --label "Alice"`.
See [invitations](docs/web-chat.md#owner-managed-invitations) and [Configuration](docs/configuration.md).

### Source-based macOS service installation

With the build requirements above and a code-signing identity named `Kodrik Local`
available in your keychain, run `./scripts/install-macos.sh` from the checkout.
The installer requires this identity and stops if signing fails. For a manual
build without service installation, use the commands above. Homebrew installations
use `brew services` instead; do not run this installer over a brew-managed service.

The installer builds and installs `~/.local/bin/kodrik`, creates a configuration
template if needed, and manages the per-user gateway LaunchAgent. On a fresh
install, fill in `~/.kodrik/config.toml`, configure Telegram and/or web, then rerun
the installer to start the gateway. Re-running installs the current checkout and
restarts the service. Logs live in `~/.kodrik/logs/`. Add `~/.local/bin` to your
shell's `PATH` for terminal use. See [Operations](docs/operations.md).

### Updates and releases

Homebrew installations update with `brew upgrade suinly/tap/kodrik`. Finish active
conversations before restarting the service; see the
[update procedure](docs/homebrew.md#update-and-restart) for preserving running
processes and verifying restart. Configuration and history remain in `~/.kodrik/`
when the package is upgraded or uninstalled.

Homebrew release binaries use ad-hoc signing, without Apple notarization.
Maintainers can follow [GitHub setup and release steps](docs/homebrew.md#maintainer-setup):
a version tag creates a tested draft release, publication opens a formula PR in
the tap, and merging that PR makes the version available through Homebrew.
Releases can also be built and verified locally before uploading their artifacts.

## Documentation

Start at the [documentation index](docs/index.md), or go directly to:

| Task                                       | Guide                                                                                |
| ------------------------------------------ | ------------------------------------------------------------------------------------ |
| Install and launch                         | [Getting started](docs/getting-started.md)                                           |
| Install with Homebrew or publish a release | [Homebrew](docs/homebrew.md)                                                         |
| Configure models or Telegram               | [Configuration](docs/configuration.md)                                               |
| Find commands and shortcuts                | [Using Kodrik](docs/usage.md)                                                        |
| Understand memory and learning             | [Memory](docs/memory.md) · [Companion](docs/companion.md)                            |
| Manage sessions and context                | [Sessions](docs/sessions.md)                                                         |
| Create reusable workflows                  | [Skills](docs/skills.md)                                                             |
| Send photos or inspect screenshots         | [Images and vision](docs/vision.md)                                                  |
| Keep diaries and structured records        | [Datasets](docs/datasets.md)                                                         |
| Create tables, charts and forms            | [UI documents](docs/ui-documents.md)                                                 |
| Publish pages and dashboards               | [Web pages](docs/web-pages.md)                                                       |
| Chat in the browser                        | [Web chat](docs/web-chat.md)                                                         |
| Search the web and read links              | [Web research](docs/web-research.md)                                                 |
| Back up state or understand crash recovery | [Storage](docs/storage.md) · [Durable recovery](docs/durable-recovery.md)            |
| Update or troubleshoot                     | [Operations](docs/operations.md) · [Troubleshooting](docs/troubleshooting.md)        |
| Contribute to the code or documentation    | [Development](docs/development.md) · [Documentation conventions](docs/publishing.md) |
