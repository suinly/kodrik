# Getting started

## Requirements

- A current stable Rust toolchain with Rust 2024 edition support and Cargo.
- Node.js 22.19+ on 22.x, 24.11+ on 24.x, or 26+, and npm to build the web frontend.
- A source checkout of Kodrik.
- Credentials for a model supporting the OpenAI Responses API. Image inputs
  require additional [vision support](vision.md).

## Build

Run from the repository root:

```sh
npm ci --prefix web
npm run build --prefix web
cargo build --release --locked
./target/release/kodrik --version
```

The executable is `target/release/kodrik`. The examples below use that path;
you can also put the executable on your `PATH` and invoke `kodrik` directly.

## Configure

Create the state directory:

```sh
mkdir -p ~/.kodrik
```

Create `~/.kodrik/config.toml` with the minimal configuration below. Replace the
model and API key with values for your provider:

```toml
[gateway]

[provider.primary]
type = "openai"
model = "your-model"
api_key = "your-key"
```

The empty `[gateway]` table is required even for terminal-only use. The file is
read from the home directory, regardless of where Kodrik starts. Values are
literal TOML strings; `${VARIABLE}` is not expanded. See the
[configuration reference](configuration.md) for custom endpoints and Telegram.

## Start a conversation

```sh
./target/release/kodrik
```

Try “Remember that I prefer concise answers”, then `/new` and “What do you
remember about me?”. Use `/sessions` to reopen an earlier terminal conversation.
For a single request:

```sh
./target/release/kodrik ask "Explain something interesting about cities."
```

Kodrik creates state files as needed. Automatic learning is enabled by default;
daily initiative requires a running, configured Telegram gateway and an active
interest. See [Companion](companion.md) to control both.

## Next steps

- [Using Kodrik](usage.md): shortcuts, cancellation and progress messages.
- [Configuration](configuration.md): Telegram, fast decision models and defaults.
- [Invitations](web-chat.md#owner-managed-invitations): manually grant and revoke access.
- [Storage and backups](storage.md): where conversations and memory are saved.
