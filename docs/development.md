# Development

## Build and test

Use a current stable Rust toolchain and Node.js 22.22.3+ on 22.x, 24.15+ on 24.x,
or 26+. From the repository root:

```sh
npm ci --prefix web
npm run build --prefix web
cargo build --locked
cargo test --locked
cargo clippy --all-targets --locked
cargo fmt --check
git diff --check
```

The Nuxt static build is embedded by the Rust build. For browser and protocol tests:

```sh
npm exec --prefix web -- playwright install chromium
npm test --prefix web
```

These tests start a local Axum server with a deterministic fake model and a
temporary data directory. They validate the real SSE snapshot reader and
Chromium flows without provider credentials. Rebuild frontend assets before
running them. Test screenshots are written to the system temporary directory.

Some tests use local network listeners for provider fixtures. An environment
that blocks loopback binding cannot run those tests successfully; rerun them
in an environment that permits local listeners. Those failures do not establish
a provider integration regression.

## Homebrew distribution

Homebrew formula generation, release validation and maintainer setup are described
in [Homebrew releases](homebrew.md#maintainer-setup). Run
`brew ruby scripts/test-homebrew.rb` to check generated formulas and launchd
settings with Homebrew's real Ruby libraries, and `actionlint` to validate the
release workflows. Neither check installs Kodrik or starts a service.

## Source map

| Path | Responsibility |
| --- | --- |
| `src/main.rs`, `src/commands/` | CLI dispatch, one-shot mode, TUI and gateway entry points |
| `src/config.rs`, `src/bootstrap.rs` | Configuration, providers, tools and service assembly |
| `src/agent/core.rs`, `runner.rs` | Agent execution and conversation lifecycle |
| `src/agent/subagents.rs`, `tools/subagents.rs` | Parent-bound child sessions, durable input, lifecycle controls and tool schemas |
| `src/agent/context.rs` | Prompt and session context snapshots |
| `src/agent/session/` | Transactional SQLite sessions, checkpoints and legacy JSON import |
| `src/agent/system_one.rs`, `command_audit.rs`, `group_reply.rs` | Independent decisions, batched tool audit and durable approvals, and group participation policy |
| `src/agent/llm/` | Provider contract and Responses API adapter |
| `src/agent/tools/` | Tool schemas, execution and registration |
| `src/agent/memory.rs`, `companion.rs`, `learning.rs` | Explicit memory and automatic learning |
| `src/agent/skills.rs`, `skills/builtin.rs` | Skill discovery and embedded-bundle installation |
| `src/gateway/` | Event handling and proactive scheduling |
| `src/interfaces/` | Interface abstraction and Telegram transport |
| `src/web/` | Page publication, owner authentication, chat API and SSE projection |
| `web/` | Nuxt 4 / Nuxt UI frontend, static build and browser tests |
| `skills/`, `build.rs` | Built-in skill bundle and rebuild tracking |

Paths abbreviated after a comma are relative to the same directory as the first
path. Most tests live next to their implementation in `#[cfg(test)]` modules.

## Request flow

1. A command or gateway submits input to the agent runner. For ordinary group
   messages the gateway first asks System One whether to reply, before any
   visible activity. Silent inputs are saved without starting the agent.
2. The foreground agent chooses the session. For an unrelated incoming topic,
   its system prompt requires `session_search`, then `switch_session` to a
   suitable existing personal dialogue or a new one if none fits. The handoff
   uses the durable run transition, not a System One routing decision.
3. The agent loads or reuses context snapshots, adds the current time in
   `companion.timezone` before each model call and executes requested tools.
   The clock is live context, not a persisted snapshot.
4. Messages and session state are persisted; substantive successful turns may
   trigger a separate learning review.

The proactive gateway path uses a separate agent with history and encyclopedia
tools, plus web search/reading when configured, with the same live clock context. It shares the primary
provider, but has bounded calls and output. See [Companion](companion.md) for its delivery semantics.

## Extending the agent

Implement the `Tool` trait under `src/agent/tools/` and register the tool in
`src/bootstrap.rs`. Registration for the foreground agent does not automatically
grant access to proactive research. Keep that tool set explicit.

Use System One for bounded choices with a clear validation and fallback policy;
group participation and command/tool review are its consumers. A new consumer should define its own
state, question, criteria and error handling rather than embedding side effects
in the decision provider.

Tool review runs before the common tool execution checkpoint, so native tools,
Bashkit, MCP and child agents use the same gate. Batch verdicts require exact ID coverage;
malformed or failed decisions require confirmation for every call in that batch.
Approval records are bound to immutable tool names and arguments and a live run in SQLite. Human
decision endpoints are session-scoped and are not exposed as model tools.
Session audit settings live separately from transcript/checkpoint payloads and
are bound to the session generation. Child agents read the current parent setting.
New session generations capture the user's profile default atomically with their
first save; the fallback is Trusted. Trusted skips review, Review batches System One
requests, and Ask requires a human decision for every shell call without a reviewer.
An Ask record is re-reviewed when switching to Review; reviewed Safe records need
human approval in Ask. Reason codes are localized at each interface boundary.
The execution-intent transaction rechecks both the verdict and session setting;
if either changed, the agent returns to the approval gate before starting a tool.
Claimed invocations no longer accept approval decisions.

Update the relevant feature page and [configuration reference](configuration.md)
when behavior or settings change. See [Documentation conventions](publishing.md) for
documentation checks and structure.

## Durable recovery fixtures

[Durable recovery](durable-recovery.md) maps each crash boundary to its executable
test and describes the optional checkpoint benchmark. The new process gates and
fault injection live in `src/agent/durable_fixture.rs` under `cfg(test)`; no
production flag can activate them. `storage_recovery_tests.rs` checks real backup,
restore and migration. These suites require disposable child processes; the
process/service fixtures also require local HTTP listeners. A sandbox denial is
not a passing test and must be rerun with appropriate execution permissions.
