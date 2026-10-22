# Homebrew installation and releases

The Homebrew package targets macOS 14 Sonoma or newer on Apple Silicon.
Intel Macs and Linux are not included. Stable releases are available through
`suinly/homebrew-tap`.

## Install and configure

```sh
brew install suinly/tap/kodrik
kodrik --version
install -d -m 700 ~/.kodrik
```

Create `~/.kodrik/config.toml` using the [configuration guide](configuration.md).
For terminal-only use, the minimum is:

```toml
[gateway]

[provider.primary]
type = "openai"
model = "your-model"
api_key = "your-key"
```

Replace the model and key, then run `chmod 600 ~/.kodrik/config.toml`.
Run `kodrik` for the TUI or `kodrik ask "Hello"` for one request. Terminal tools
use the directory from which you launch Kodrik. Rust, Node.js and a source
checkout are not required; web assets and built-in skills are in the executable.

Installation does not create credentials or start the gateway. Upgrading and
uninstalling do not overwrite or delete `~/.kodrik/`.

## Run the background gateway

Configure Telegram and/or web first, following [Getting started](getting-started.md#configure).
For web password login, run `kodrik web set-password`. Create private directories,
then start the gateway:

```sh
install -d -m 700 ~/.kodrik/workspace ~/.kodrik/logs
brew services start suinly/tap/kodrik
brew services info suinly/tap/kodrik
```

Run these commands as your regular user, without `sudo`. The gateway starts at
login with the launchd label `com.kodrik.gateway` and `KeepAlive`. Its executable
uses Homebrew's stable `opt/kodrik/bin/kodrik` path. The working directory is
`~/.kodrik/workspace`; logs are `~/.kodrik/logs/kodrik-gateway.log` and
`~/.kodrik/logs/kodrik-gateway.err.log`.

The service PATH includes `~/.local/bin`, `~/.cargo/bin`, Homebrew bin/sbin and
system tools. It does not load your interactive shell configuration. For tools
installed elsewhere, use absolute paths or configure their documented paths,
such as `research.obscura_path`. Optional Obscura and SearXNG are not installed
by the formula.

## Update and restart

From a separate terminal, allow the current conversation to finish, then:

```sh
brew update
HOMEBREW_NO_INSTALL_CLEANUP=1 brew upgrade suinly/tap/kodrik
kodrik --version
brew services restart suinly/tap/kodrik
brew services info suinly/tap/kodrik
```

Only restart the service if you use it. Existing CLI/TUI processes continue to
run their old executable; exit and relaunch them to use the update. After the
new service has started successfully and no process uses the old version, run
`brew cleanup suinly/tap/kodrik`.

The bundled `self-update` skill detects Homebrew installations and updates through
Homebrew. When invoked inside the gateway, it uses `request-restart <pid>` so the
response can be delivered before the old process exits. It does not rebuild from
Git or invoke `brew services restart` inside its own conversation. See
[Gateway lifecycle](operations.md#gateway-lifecycle) for confirmation and recovery.
Use `/new` after upgrading to refresh the conversation's bundled skill catalog.

## Intermediate versions from main

Install the latest committed code without waiting for a release:

```sh
brew update
brew install --HEAD suinly/tap/kodrik
brew list --versions suinly/tap/kodrik
```

HEAD builds locally from `main`. Homebrew installs Rust and Node.js 24 as build
dependencies, builds the embedded web frontend and executable, then ad-hoc signs
it with the same gateway identifier as releases. Building takes longer than
installing a stable release. Uncommitted changes and commits outside `main` are
not included. Platform requirements are the same as for stable.

To update an installed HEAD build from a separate terminal:

```sh
brew update
HOMEBREW_NO_INSTALL_CLEANUP=1 brew upgrade --fetch-HEAD suinly/tap/kodrik
brew list --versions suinly/tap/kodrik
brew services restart suinly/tap/kodrik
```

Only restart if you use the gateway. Follow the same restart and cleanup checks
as stable updates. Homebrew reports a version such as `HEAD-a1b2c3d` identifying
the source revision; `kodrik --version` still reports the Cargo package version,
which may stay unchanged between commits. The `self-update` skill preserves the
installed stable or HEAD channel and uses `--fetch-HEAD` for HEAD updates.

To switch an existing installation between channels, finish active conversations
and exit CLI/TUI processes. From a separate terminal, stop the service if used,
remove the installed kegs, then install the desired channel:

```sh
brew services stop suinly/tap/kodrik
brew uninstall --force suinly/tap/kodrik
brew install --HEAD suinly/tap/kodrik
brew services start suinly/tap/kodrik
```

For a return to stable, omit `--HEAD`. Omit the service commands for terminal-only
use. Both channels use the same `~/.kodrik/` data and service; uninstalling keeps
that data. Back it up before testing intermediate builds using [Storage](storage.md).

## Migrate from the source installer

Keep your existing `~/.kodrik/` and back it up following [Storage](storage.md).
Install the brew package first, then finish active conversations and stop the
old service. Obtain your numeric user ID with `id -u`, inspect
`launchctl print gui/<uid>/com.kodrik.gateway`, and verify that it runs your
source-installed `~/.local/bin/kodrik`. Then:

```sh
launchctl bootout gui/<uid>/com.kodrik.gateway
mv ~/Library/LaunchAgents/com.kodrik.gateway.plist ~/.kodrik/gateway.source.plist.backup
mv ~/.local/bin/kodrik ~/.local/bin/kodrik.source-backup
```

Replace `<uid>` with the verified numeric ID; run the `mv` commands only if those
files exist and the backup destinations do not already exist. Preserve the old
checkout if it contains files the gateway was working on. The new gateway's
default working directory changes to `~/.kodrik/workspace`.

Create the workspace/log directories as above, then start the brew service and
check `command -v kodrik`, `kodrik --version` and the service logs. Do not run
`scripts/install-macos.sh` for the brew installation: it reinstalls the separate
source binary and owns the same launchd label.

## Stop and uninstall

```sh
brew services stop suinly/tap/kodrik
brew uninstall suinly/tap/kodrik
```

Close CLI/TUI processes too. Configuration, keys, conversations, attachments,
skills, workspace files and logs remain in `~/.kodrik/`.

## Signing

Release binaries are explicitly ad-hoc signed with identifier
`com.kodrik.gateway`, then verified before packaging. No personal `Kodrik Local`
certificate is required. This is not Developer ID signing or Apple notarization;
the signature does not authenticate an Apple-verified publisher. Gatekeeper may
block a quarantined manual download, and macOS privacy permissions may need to
be granted again after updates. The SHA-256 in the formula verifies the release
archive's bytes. The source installer retains its existing `Kodrik Local` policy.

## Maintainer setup

Create a public `suinly/homebrew-tap` repository with an initial README and a
`main` branch. The formula will live at `Formula/kodrik.rb`. In the Kodrik
repository's Actions secrets, set `HOMEBREW_TAP_TOKEN` to a fine-grained token
restricted to the tap, with **Contents: read/write** and
**Pull requests: read/write**. The ordinary `GITHUB_TOKEN` cannot push to a
different repository. Do not put tokens in either repository.

### Release process

1. Merge the release changes into `main`. Update `Cargo.toml` and the Kodrik
   package entry in `Cargo.lock` when releasing a new version.
2. Push a stable tag matching the binary version, for example `v0.1.2` for
   `kodrik 0.1.2`. Tags and release archives must not be moved or replaced.
3. **Release macOS** builds on the macOS 15 ARM runner with deployment target
   14.0, builds the frontend, runs Rust and installer/formula checks, builds
   the release binary, rejects non-system dynamic dependencies, signs it,
   packages it and generates the checksummed formula.
4. The job installs/tests/uninstalls the package in a temporary local tap without
   starting a service. It then creates a **draft** GitHub release containing
   `kodrik-X.Y.Z-aarch64-apple-darwin.tar.gz`, `SHA256SUMS` and `kodrik.rb`.
5. Review the draft and publish it. **Update Homebrew tap** downloads that release
   archive, calculates its checksum and opens a versioned formula PR in the tap.
6. Review and merge the tap PR. Users can now install or upgrade to that version.

The build can also run locally from the clean tagged checkout, using the same
build, packaging, signature and Homebrew checks in `.github/workflows/release.yml`.
Upload the archive, `SHA256SUMS` and generated `kodrik.rb` to a draft release for
that existing tag, then publish it. Publication triggers the same tap workflow;
the tap update itself runs on Linux and does not need a macOS build runner.

A failed build or install check prevents draft creation. An existing tag/release
is not overwritten on rerun. A failed tap update leaves the published release
available and the tap at its previous version; fix the cause and rerun the failed
job. If the update branch already exists, update its PR rather than force-pushing
over a reviewed branch. Prereleases do not update the tap.

To generate a formula manually from the downloaded archive:

```sh
ruby scripts/homebrew-formula.rb v0.1.2 kodrik-0.1.2-aarch64-apple-darwin.tar.gz > kodrik.rb
brew ruby scripts/test-homebrew.rb
```

Copy the generated file into the tap's `Formula/kodrik.rb` and review its version,
URL and checksum before merging. The generator accepts stable `vX.Y.Z` tags only.
