---
name: self-update
description: Use when the user asks Kodrik to update itself, build its binary, or restart its running gateway.
---

# Update, Build, or Restart Kodrik

Follow the requested operation:

- **Update yourself / update Kodrik:** identify the installation below, update through its package manager or Git checkout, then restart the verified gateway. The update request authorizes these steps; do not ask separately for permission to pull or upgrade.
- **Restart:** restart the existing gateway binary, without pulling or building.
- **Build / rebuild:** build the current local revision, without pulling or restarting unless also requested.

Explicit restrictions in the user's request take precedence. Reading this skill alone does not authorize any operation. Never reset, stash, discard changes, switch branches, or create merge commits as part of this workflow.

## Identify the installation and gateway

For update or restart, inspect the running Kodrik process and its launchd job.
Obtain the numeric user ID with `id -u`, then run
`launchctl print gui/<uid>/com.kodrik.gateway` using that ID. Verify the actual
executable, PID, working directory and `KeepAlive`; do not identify an installation
from whichever `kodrik` happens to be first on `PATH`.

**Homebrew:** compare the verified executable with
`brew --prefix suinly/tap/kodrik` and its resolved Cellar path, and confirm the
installed formula with `brew list --versions suinly/tap/kodrik`. The gateway job
must launch the stable `opt/kodrik/bin/kodrik gateway` path with `KeepAlive`.
Follow the Homebrew section below; a checkout is not required.

**Source install:** follow the Git/build sections. From the repository root,
inspect `git status --short --branch` and `git log -1 --oneline`. Confirm the job
runs `~/.local/bin/kodrik gateway`, uses this checkout as its working directory,
and has `KeepAlive`. Legacy jobs may run `target/release/kodrik gateway`; verify
the actual executable before proceeding. For build-only requests, use the
explicitly requested checkout; a local build does not replace a brew installation.

## Update with Homebrew

For an update request, determine the installed channel from the verified
executable's resolved Cellar keg and its `INSTALL_RECEIPT.json` (`source.spec`).
Use `brew list --versions suinly/tap/kodrik` to corroborate it: HEAD kegs have
versions such as `HEAD-a1b2c3d`. Multiple retained kegs do not identify the active
channel; if the active keg or receipt is unclear, stop and report it.

Run `brew update`, then preserve that channel:

- **stable:** `HOMEBREW_NO_INSTALL_CLEANUP=1 brew upgrade suinly/tap/kodrik`
- **head:** `HOMEBREW_NO_INSTALL_CLEANUP=1 brew upgrade --fetch-HEAD suinly/tap/kodrik`

Stop on failure; do not fall back to Git, rebuild, local signing or direct Cellar
writes. Never switch channels during an update. Use the updated
`opt/kodrik/bin/kodrik --version` to verify the installed executable. For HEAD,
also record the new resolved keg/revision: the CLI package version can stay the
same between commits. Report an unchanged revision as already current.
Keeping the old keg until restart lets the running process finish using its
current executable, including subprocesses it may need during the final response.

For restart-only requests, skip the update. Use the verified stable executable
for `request-restart <verified-pid>` when this gateway handles the conversation,
then follow the confirmation checks in the Restart section. Do not run
`brew services restart` from inside that gateway: it can terminate the response.
Outside the gateway, use `brew services restart suinly/tap/kodrik` only after
verifying this is the service being controlled. Never run Homebrew as root.

Run `brew cleanup suinly/tap/kodrik` only after the new PID has confirmed startup
and no CLI/TUI or gateway process still uses the old keg. If the update remains
pending or confirmation is missing, retain it and report the observed state.

## Update from Git (update requests only)

1. Require a clean working tree, including the index and untracked files, using `git status --porcelain`. If there are local changes, stop the update and report them; do not build or restart as a fallback.
2. Verify an attached branch with `git symbolic-ref --quiet --short HEAD` and a configured upstream with `git rev-parse --abbrev-ref --symbolic-full-name '@{upstream}'`. If either is absent, stop and explain what is missing. Do not guess a remote or branch.
3. Run `git fetch` for that branch's configured upstream remote. If fetching fails, stop; do not claim the local checkout is up to date. Compare fresh upstream state using `git rev-list --left-right --count 'HEAD...@{upstream}'`. If HEAD has commits absent from upstream (the first count is nonzero), stop and report the local commits or divergence.
4. Run `git pull --ff-only` to obtain the upstream changes. If it fails, stop without building or restarting. Record the resulting revision with `git rev-parse HEAD`. Even if already current, continue with the requested build and restart. Never substitute a local rebuild for a failed Git update.

## Build (update or build requests)

Run `npm ci --prefix web`, then `npm run build --prefix web`, then `cargo build --release --locked` from the verified repository root. Node.js 22.19+ on 22.x, 24.11+ on 24.x, or 26+ is required to build the embedded web frontend. If any build step fails, stop without restarting the running gateway. Verify that `target/release/kodrik` exists and record the revision used for the build. An explicitly requested local build may include uncommitted changes; report that fact and preserve them.

For an update using the installed `~/.local/bin/kodrik`, sign and verify the successful build before replacing the executable or requesting restart:

```sh
cp target/release/kodrik ~/.local/bin/kodrik.new &&
codesign --force --sign "Kodrik Local" --identifier com.kodrik.gateway --timestamp=none ~/.local/bin/kodrik.new &&
codesign --verify --strict ~/.local/bin/kodrik.new &&
mv ~/.local/bin/kodrik.new ~/.local/bin/kodrik
```

Use the same `Kodrik Local` certificate and signing identifier as the macOS installer. If copying, signing, verification, or replacement fails, stop without requesting restart; report the error. Never fall back to an unsigned or ad-hoc-signed replacement. The rename replaces the executable atomically. A build-only request does not install or restart. Never run the installer from inside the gateway: it stops the service before replacing its LaunchAgent.

## Restart (update or restart requests)

Tell the user that restarting may interrupt the current response. If the verified gateway is handling this conversation, run the verified installed executable with `request-restart <verified-pid>` (the stable `opt/kodrik/bin/kodrik` path for Homebrew, or `~/.local/bin/kodrik` for the source installer). Report the restart as requested, not completed: the gateway saves and sends the final response, exits cleanly, and the next process edits that response with confirmation.

Never defer a restart with `launchctl submit`, `nohup`, a detached/background shell command, a fixed-delay signal, or a temporary/persistent plist change. These mechanisms can outlive the response, acquire an implicit `KeepAlive`, or restart the gateway repeatedly.

When the request is running outside the verified gateway process (for example in the TUI), use `brew services restart suinly/tap/kodrik` for Homebrew, or restart only the verified source-installed job with `launchctl kickstart -k gui/<uid>/com.kodrik.gateway`. Do not kill unrelated processes or use a broad `pkill`.

After restarting, check the launchd job, new PID, binary path, and configured log for startup errors. Report the package/Git update, build, restart request, and restart confirmation separately, omitting steps that were not requested. If confirmation is absent, inspect `~/.kodrik/restart-request.json` and `~/.kodrik/restart-receipt.json`; never schedule a second restart over pending state.
