---
name: self-update
description: Use when the user asks Kodrik to update itself, build its binary, or restart its running gateway.
---

# Update, Build, or Restart Kodrik

Follow the requested operation:

- **Update yourself / update Kodrik:** fetch and fast-forward the configured upstream, build the release binary, then restart the verified gateway. The update request authorizes these steps; do not ask separately for permission to pull.
- **Restart:** restart the existing gateway binary, without pulling or building.
- **Build / rebuild:** build the current local revision, without pulling or restarting unless also requested.

Explicit restrictions in the user's request take precedence. Reading this skill alone does not authorize any operation. Never reset, stash, discard changes, switch branches, or create merge commits as part of this workflow.

## Identify the checkout and gateway

From the repository root, inspect `git status --short --branch` and `git log -1 --oneline`. For update or restart, inspect the running Kodrik process and its launchd job. Obtain the numeric user ID with `id -u`, then run `launchctl print gui/<uid>/com.kodrik.gateway` using that ID. Confirm that the job runs `~/.local/bin/kodrik gateway`, uses this checkout as its working directory, and has `KeepAlive`. Legacy jobs may run `target/release/kodrik gateway`; verify the actual executable before proceeding.

## Update from Git (update requests only)

1. Require a clean working tree, including the index and untracked files, using `git status --porcelain`. If there are local changes, stop the update and report them; do not build or restart as a fallback.
2. Verify an attached branch with `git symbolic-ref --quiet --short HEAD` and a configured upstream with `git rev-parse --abbrev-ref --symbolic-full-name '@{upstream}'`. If either is absent, stop and explain what is missing. Do not guess a remote or branch.
3. Run `git fetch` for that branch's configured upstream remote. If fetching fails, stop; do not claim the local checkout is up to date. Compare fresh upstream state using `git rev-list --left-right --count 'HEAD...@{upstream}'`. If HEAD has commits absent from upstream (the first count is nonzero), stop and report the local commits or divergence.
4. Run `git pull --ff-only` to obtain the upstream changes. If it fails, stop without building or restarting. Record the resulting revision with `git rev-parse HEAD`. Even if already current, continue with the requested build and restart. Never substitute a local rebuild for a failed Git update.

## Build (update or build requests)

Run `npm ci --prefix web`, then `npm run build --prefix web`, then `cargo build --release --locked` from the verified repository root. Node.js 22.12+ is required to build the embedded web frontend. If any build step fails, stop without restarting the running gateway. Verify that `target/release/kodrik` exists and record the revision used for the build. An explicitly requested local build may include uncommitted changes; report that fact and preserve them.

For an update using the installed `~/.local/bin/kodrik`, sign and verify the successful build before replacing the executable or requesting restart:

```sh
cp target/release/kodrik ~/.local/bin/kodrik.new &&
codesign --force --sign "Kodrik Local" --identifier com.kodrik.gateway --timestamp=none ~/.local/bin/kodrik.new &&
codesign --verify --strict ~/.local/bin/kodrik.new &&
mv ~/.local/bin/kodrik.new ~/.local/bin/kodrik
```

Use the same `Kodrik Local` certificate and signing identifier as the macOS installer. If copying, signing, verification, or replacement fails, stop without requesting restart; report the error. Never fall back to an unsigned or ad-hoc-signed replacement. The rename replaces the executable atomically. A build-only request does not install or restart. Never run the installer from inside the gateway: it stops the service before replacing its LaunchAgent.

## Restart (update or restart requests)

Tell the user that restarting may interrupt the current response. If the verified gateway is handling this conversation, run `./target/release/kodrik request-restart <verified-pid>`. Report the restart as requested, not completed: the gateway saves and sends the final response, exits cleanly, and the next process edits that response with confirmation.

Never defer a restart with `launchctl submit`, `nohup`, a detached/background shell command, a fixed-delay signal, or a temporary/persistent plist change. These mechanisms can outlive the response, acquire an implicit `KeepAlive`, or restart the gateway repeatedly.

When the request is running outside the verified gateway process (for example in the TUI), restart only the verified job with `launchctl kickstart -k gui/<uid>/com.kodrik.gateway`. Do not kill unrelated processes or use a broad `pkill`.

After restarting, check the launchd job, new PID, binary path, and configured log for startup errors. Report Git update, build, restart request, and restart confirmation separately, omitting steps that were not requested. If confirmation is absent, inspect `~/.kodrik/restart-request.json` and `~/.kodrik/restart-receipt.json`; never schedule a second restart over pending state.
